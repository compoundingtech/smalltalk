//! Attach this terminal to a terminal that another fleet host owns, PTY to PTY over Fabric.
//!
//! `st terminals expose-fabric` has the owning host's Fabric forward each tunnel under the fleet's
//! PTY protocol to the owner daemon's terminal socket (`TerminalSocket`), one connection per
//! tunnel. `expose-fabric --exec`, and any exposure made before the socket existed, runs
//! `st terminals serve-fabric --stdio` for each tunnel instead (`serve_stdio`), which needs no
//! daemon. The dialing CLI sends one route line that names the PTY session, the
//! subject, and the runtime incarnation st selected. The owner proves that
//! incarnation against the PTY itself, as a local attach does, answers `{"ok":true}`, and then
//! splices the tunnel to the session socket: the attach client speaks the PTY session protocol to
//! the session. The route line is the `pty remote-serve` control line with two more fields.

use std::io::{BufRead as _, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use pty_client::{ClientIo, Reconnect, RouteRefusedError};

pub use st3_terminal_direct::{
    FabricTarget, ROUTE_LINE_LIMIT, ROUTE_TIMEOUT, RouteAnswer, RouteError, RouteRequest,
    open_route, owner_peer, protocol,
};

use crate::model::LocalTerminal;

/// Attach this terminal to `request`'s PTY session over Fabric until the person detaches or the
/// session ends. A lost tunnel is dialed again with the same incarnation until the owner refuses.
/// `Err(RouteError)` inside means no route opened at all, so nothing was attached.
pub async fn attach(
    target: &FabricTarget,
    request: &RouteRequest,
) -> Result<Result<i32, RouteError>> {
    attach_with_io(target, request, ClientIo::default()).await
}

async fn attach_with_io(
    target: &FabricTarget,
    request: &RouteRequest,
    io: ClientIo,
) -> Result<Result<i32, RouteError>> {
    let stream = match open_route(target, request).await {
        Ok(stream) => stream,
        Err(error) => return Ok(Err(error)),
    };
    let handle = tokio::runtime::Handle::current();
    let (target, again) = (target.clone(), request.clone());
    let reconnect: Reconnect =
        Box::new(move || match handle.block_on(open_route(&target, &again)) {
            Ok(stream) => Ok(Some(stream)),
            Err(RouteError::Refused(reason)) => Err(RouteRefusedError(reason)),
            Err(RouteError::Unreachable(_)) => Ok(None),
        });
    let code =
        crate::client::proxy_stream_with_io(&request.name, stream, Some(reconnect), io).await?;
    Ok(Ok(code))
}

/// Serve one Fabric tunnel on stdin and stdout: read a route line, prove that the named PTY
/// session under `pty_root` is the subject's terminal at the requested incarnation, answer, and
/// then splice the tunnel to the session until either side closes. It never starts or restarts
/// a session.
pub async fn serve_stdio(pty_root: &Path) -> Result<()> {
    // One thread reads stdin from the route line to the end, so no byte of the session protocol
    // waits in a buffer that the splice cannot see.
    let (lines, line) = std::sync::mpsc::channel();
    let (splice, session) = std::sync::mpsc::channel::<StdUnixStream>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut request = Vec::new();
        let read = (&mut stdin)
            .take(ROUTE_LINE_LIMIT as u64)
            .read_until(b'\n', &mut request);
        if lines.send(read.map(|_| request)).is_err() {
            return;
        }
        if let Ok(mut session) = session.recv() {
            let _ = pump(&mut stdin, &mut session);
            let _ = session.shutdown(std::net::Shutdown::Write);
        }
    });
    // Unbuffered, so every screen reaches the tunnel when the session writes it.
    let mut stdout = std::fs::File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
    let request = tokio::task::spawn_blocking(move || line.recv_timeout(ROUTE_TIMEOUT))
        .await?
        .context("no route line arrived")?
        .context("read the route line")?;
    let stream = match route(pty_root, &request).await {
        Ok(stream) => stream,
        Err(error) => {
            let answer = RouteAnswer {
                error: Some(format!("{error:#}")),
                ..RouteAnswer::default()
            };
            writeln!(stdout, "{}", serde_json::to_string(&answer)?)?;
            return Ok(());
        }
    };
    let answer = RouteAnswer {
        ok: Some(true),
        ..RouteAnswer::default()
    };
    writeln!(stdout, "{}", serde_json::to_string(&answer)?)?;
    splice
        .send(stream.try_clone()?)
        .context("the tunnel closed before the splice")?;
    let mut from_session = stream;
    // A closed tunnel is how the attach client leaves, so neither direction's end is an error.
    tokio::task::spawn_blocking(move || {
        let _ = pump(&mut from_session, &mut stdout);
    })
    .await?;
    Ok(())
}

/// The daemon's Fabric terminal service: `st terminals expose-fabric` has Fabric forward each
/// tunnel to this socket, one connection per tunnel, so no process or pipe exists per attach.
///
/// It runs on its own thread and runtime and reads nothing but the PTY registry and the kernel's
/// socket peer, so a daemon busy with the store or other requests cannot delay an attach.
pub struct TerminalSocket {
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl TerminalSocket {
    /// Bind `socket`, replacing a stale file but never a live listener, and serve PTY sessions
    /// under `pty_root` on a dedicated thread until the process ends.
    pub fn start(socket: &Path, pty_root: PathBuf) -> Result<Self> {
        Self::start_with(socket, pty_root, ROUTE_TIMEOUT)
    }

    /// As `start`, closing a tunnel that sends no route line within `route_timeout`.
    fn start_with(socket: &Path, pty_root: PathBuf, route_timeout: Duration) -> Result<Self> {
        let listener = bind_terminal_socket(socket)?;
        let live = std::sync::Arc::<std::sync::atomic::AtomicUsize>::default();
        let counted = live.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build the terminal socket runtime")?;
        std::thread::Builder::new()
            .name("st3-terminal-socket".into())
            .spawn(move || {
                runtime.block_on(crate::task_cpu::account("terminal.socket", async move {
                    match tokio::net::UnixListener::from_std(listener) {
                        Ok(listener) => {
                            serve_terminal_socket(listener, pty_root, route_timeout, counted).await
                        }
                        Err(error) => eprintln!("st3: serve the terminal socket: {error}"),
                    }
                }));
            })
            .context("start the terminal socket thread")?;
        Ok(Self { live })
    }

    /// Connections that are routing or attached right now.
    pub fn live(&self) -> usize {
        self.live.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn bind_terminal_socket(socket: &Path) -> Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    crate::config::validate_unix_socket_path(socket, "--state-dir")?;
    // Only this account may reach the socket, or even its name.
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // A second daemon must never detach an active listener by unlinking its pathname.
    anyhow::ensure!(
        StdUnixStream::connect(socket).is_err(),
        "refusing to replace live Unix socket listener at {}",
        socket.display()
    );
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = std::os::unix::net::UnixListener::bind(socket)
        .with_context(|| format!("bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn serve_terminal_socket(
    listener: tokio::net::UnixListener,
    pty_root: PathBuf,
    route_timeout: Duration,
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let pty_root = std::sync::Arc::new(pty_root);
    loop {
        let tunnel = match listener.accept().await {
            Ok((tunnel, _)) => tunnel,
            // Running out of file descriptors fails one accept; it must not end the service.
            Err(error) => {
                eprintln!("st3: accept a terminal connection: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (pty_root, live) = (pty_root.clone(), live.clone());
        live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tokio::spawn(crate::task_cpu::account("terminal.tunnel", async move {
            serve_tunnel(tunnel, &pty_root, route_timeout).await;
            live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }));
    }
}

/// One tunnel: a route line, an answer, and then the session protocol both ways until the
/// session ends. The attach client leaving closes its sending side; the session sees that and
/// closes, which ends the tunnel.
async fn serve_tunnel(
    mut tunnel: tokio::net::UnixStream,
    pty_root: &Path,
    route_timeout: Duration,
) {
    use tokio::io::AsyncWriteExt as _;
    let line = match tokio::time::timeout(route_timeout, read_route_line(&mut tunnel)).await {
        Ok(Ok(line)) => line,
        Ok(Err(_)) | Err(_) => return,
    };
    let session = match route(pty_root, &line).await.and_then(|session| {
        session.set_nonblocking(true)?;
        Ok(tokio::net::UnixStream::from_std(session)?)
    }) {
        Ok(session) => session,
        Err(error) => {
            let answer = RouteAnswer {
                error: Some(format!("{error:#}")),
                ..RouteAnswer::default()
            };
            let _ = tunnel.write_all(&answer_line(&answer)).await;
            return;
        }
    };
    let answer = RouteAnswer {
        ok: Some(true),
        ..RouteAnswer::default()
    };
    if tunnel.write_all(&answer_line(&answer)).await.is_err() {
        return;
    }
    splice(tunnel, session).await;
}

fn answer_line(answer: &RouteAnswer) -> Vec<u8> {
    let mut line = serde_json::to_vec(answer).expect("a route answer serializes");
    line.push(b'\n');
    line
}

/// The route line, read one byte at a time so every byte after it stays for the session.
async fn read_route_line(tunnel: &mut tokio::net::UnixStream) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;
    let mut line = Vec::new();
    loop {
        let byte = tunnel.read_u8().await.context("read the route line")?;
        if byte == b'\n' {
            return Ok(line);
        }
        anyhow::ensure!(line.len() < ROUTE_LINE_LIMIT, "the route line is too long");
        line.push(byte);
    }
}

/// Copy both ways until the session stops sending: the tunnel's end is passed on as the end of
/// the session's input, and the session then closes.
async fn splice(tunnel: tokio::net::UnixStream, session: tokio::net::UnixStream) {
    use tokio::io::AsyncWriteExt as _;
    let (mut from_tunnel, mut to_tunnel) = tunnel.into_split();
    let (mut from_session, mut to_session) = session.into_split();
    let input = async {
        let _ = tokio::io::copy(&mut from_tunnel, &mut to_session).await;
        let _ = to_session.shutdown().await;
        std::future::pending::<()>().await
    };
    let output = async {
        let _ = tokio::io::copy(&mut from_session, &mut to_tunnel).await;
        let _ = to_tunnel.shutdown().await;
    };
    tokio::select! {
        () = input => {}
        () = output => {}
    }
}

/// Copy `from` to `to` until `from` ends, one bounded read and one write at a time.
///
/// Not `std::io::copy`: on Linux it splices a socket into a pipe, and a splice from an idle
/// session socket sleeps in the kernel holding the stdout pipe's lock, so Fabric's read of that
/// pipe blocks uninterruptibly, even when nonblocking, until the terminal next prints.
fn pump(from: &mut impl std::io::Read, to: &mut impl std::io::Write) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => to.write_all(&buffer[..read])?,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// The PTY session a route line names, connected and proven: it is tagged as the subject's
/// terminal, and the kernel and the registry prove the incarnation as a local attach does.
async fn route(pty_root: &Path, line: &[u8]) -> Result<StdUnixStream> {
    let request: RouteRequest = serde_json::from_slice(line).context("parse the route line")?;
    anyhow::ensure!(
        request.op == "route",
        "unsupported operation `{}`",
        request.op
    );
    anyhow::ensure!(
        is_session_name(&request.name),
        "`{}` is not a PTY session name",
        request.name
    );
    let metadata = pty_core::registry::read_metadata_in(pty_root, &request.name)
        .with_context(|| format!("no PTY session `{}` runs here", request.name))?;
    let tagged = metadata
        .tags
        .as_ref()
        .and_then(|tags| tags.get("st3.subject"))
        .is_some_and(|subject| *subject == request.subject);
    anyhow::ensure!(
        tagged,
        "PTY session `{}` is not `{}`'s terminal",
        request.name,
        request.subject
    );
    crate::client::open_local_terminal(&LocalTerminal {
        subject: request.subject,
        runtime_id: request.name,
        incarnation_id: request.incarnation,
        pty_root: PathBuf::from(pty_root),
    })
    .await
}

/// A PTY session name is one plain file name under the PTY root.
fn is_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fabric reads a tunnel child's stdout on its runtime. While the session is idle the pump
    /// waits on the session socket, and that wait must leave the stdout pipe free: a splice
    /// would hold the pipe's lock, so any other use of the pipe would block until the session
    /// next writes.
    #[test]
    fn an_idle_session_leaves_the_tunnel_pipe_usable() {
        let (mut session, mut owner) = StdUnixStream::pair().unwrap();
        let (mut tunnel, mut stdout) = std::io::pipe().unwrap();
        let mut probe = stdout.try_clone().unwrap();
        let pumping = std::thread::spawn(move || pump(&mut session, &mut stdout));
        std::thread::sleep(std::time::Duration::from_millis(200));
        let (probed, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || probed.send(probe.write_all(b"probe")));
        let usable = done.recv_timeout(std::time::Duration::from_secs(5));
        // Release the pump either way so a failure ends instead of hanging the suite.
        owner.write_all(b"screen").unwrap();
        drop(owner);
        assert!(
            matches!(usable, Ok(Ok(()))),
            "a write to the tunnel pipe blocked while the pump waited on an idle session"
        );
        pumping.join().unwrap().unwrap();
        let mut seen = Vec::new();
        tunnel.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, b"probescreen");
    }

    #[test]
    fn a_route_names_one_file_under_the_pty_root() {
        assert!(is_session_name("fleet.example.worker.0123abcd"));
        for name in ["", ".", "..", "../pty/other", "a/b", "nul\0byte"] {
            assert!(!is_session_name(name), "{name:?}");
        }
        assert!(!is_session_name(&"a".repeat(256)));
    }

    #[test]
    fn a_route_line_is_the_pty_remote_line_with_a_subject_and_an_incarnation() {
        let line = serde_json::to_value(RouteRequest::new(
            "example-worker",
            "agent/example/worker",
            "42:2026-09-29T08:00:00.000Z",
        ))
        .unwrap();
        assert_eq!(
            line,
            serde_json::json!({
                "op": "route",
                "name": "example-worker",
                "subject": "agent/example/worker",
                "incarnation": "42:2026-09-29T08:00:00.000Z",
            })
        );
    }

    #[tokio::test]
    async fn a_route_outside_the_pty_root_or_to_another_subject_is_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("example-other.json"),
            serde_json::json!({
                "createdAt": "2026-09-29T08:00:00.000Z",
                "tags": { "st3.subject": "agent/example/other" },
            })
            .to_string(),
        )
        .unwrap();
        let refused = |name: &str| {
            serde_json::to_vec(&RouteRequest::new(
                name,
                "agent/example/worker",
                "42:2026-09-29T08:00:00.000Z",
            ))
            .unwrap()
        };
        let outside = route(root.path(), &refused("../example-other")).await;
        assert!(
            format!("{:#}", outside.unwrap_err()).contains("is not a PTY session name"),
            "a name must not leave the PTY root"
        );
        let other = route(root.path(), &refused("example-other")).await;
        assert!(
            format!("{:#}", other.unwrap_err()).contains("is not `agent/example/worker`'s"),
            "a session tagged for another subject is refused"
        );
        let missing = route(root.path(), &refused("example-missing")).await;
        assert!(format!("{:#}", missing.unwrap_err()).contains("no PTY session"));
        let list = route(root.path(), br#"{"op":"list"}"#).await;
        assert!(list.is_err());
    }

    mod terminal_socket {
        //! The daemon's terminal socket, reached directly as Fabric's socket exposure reaches
        //! it: Fabric forwards each tunnel's bytes untouched.

        use std::io::{Read as _, Write as _};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        use pty_core::protocol::{MessageType, PacketReader};

        use super::super::*;

        const SUBJECT: &str = "agent/example/worker";
        const CREATED_AT: &str = "2026-09-29T08:00:00.000Z";
        const SCREEN: &[u8] = b"the worker's screen";

        /// A stand-in PTY session served by this process, so its socket peer is the incarnation's
        /// PTY daemon. It answers each ATTACH with a screen and closes a client that detaches or
        /// leaves, as a `pty` session does.
        struct Session {
            /// Clients connected right now.
            open: Arc<AtomicUsize>,
            /// ATTACH packets received in all.
            attaches: Arc<AtomicUsize>,
        }

        /// `flood` keeps writing output to each attached client until it leaves.
        fn session(pty_root: &Path, name: &str, flood: bool) -> Session {
            std::fs::create_dir_all(pty_root).unwrap();
            std::fs::write(
                pty_root.join(format!("{name}.json")),
                serde_json::json!({
                    "createdAt": CREATED_AT,
                    "tags": { "st3.subject": SUBJECT },
                })
                .to_string(),
            )
            .unwrap();
            let listener = UnixListener::bind(pty_root.join(format!("{name}.sock"))).unwrap();
            let (open, attaches) = (Arc::<AtomicUsize>::default(), Arc::<AtomicUsize>::default());
            let session = Session {
                open: open.clone(),
                attaches: attaches.clone(),
            };
            std::thread::spawn(move || {
                for client in listener.incoming() {
                    let Ok(client) = client else { return };
                    let (open, attaches) = (open.clone(), attaches.clone());
                    open.fetch_add(1, Ordering::SeqCst);
                    std::thread::spawn(move || {
                        serve_client(client, flood, &attaches);
                        open.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            });
            session
        }

        fn serve_client(mut client: UnixStream, flood: bool, attaches: &AtomicUsize) {
            let mut reader = PacketReader::new();
            let mut bytes = [0_u8; 4096];
            loop {
                let count = client.read(&mut bytes).unwrap_or(0);
                if count == 0 {
                    break;
                }
                let Ok(packets) = reader.feed(&bytes[..count]) else {
                    break;
                };
                if packets
                    .iter()
                    .any(|packet| packet.type_ == MessageType::Detach)
                {
                    break;
                }
                if packets
                    .iter()
                    .any(|packet| packet.type_ == MessageType::Attach)
                {
                    attaches.fetch_add(1, Ordering::SeqCst);
                    let mut screen = client.try_clone().unwrap();
                    std::thread::spawn(move || {
                        let output = pty_core::protocol::encode_data(&[b'x'; 64 * 1024]);
                        let _ = screen.write_all(&pty_core::protocol::encode_screen(SCREEN));
                        while flood && screen.write_all(&output).is_ok() {}
                    });
                }
            }
            let _ = client.shutdown(std::net::Shutdown::Both);
        }

        fn incarnation() -> String {
            format!("{}:{CREATED_AT}", std::process::id())
        }

        /// The service on a fresh socket over a fresh PTY root.
        fn service(route_timeout: Duration) -> (tempfile::TempDir, PathBuf, TerminalSocket) {
            let root = tempfile::tempdir().unwrap();
            let socket = root.path().join("run/terminals.sock");
            let service =
                TerminalSocket::start_with(&socket, root.path().join("pty"), route_timeout)
                    .unwrap();
            (root, socket, service)
        }

        /// Send the route line for session `name` and return the owner's answer.
        fn route_to(socket: &Path, name: &str, incarnation: &str) -> (UnixStream, String) {
            let mut tunnel = UnixStream::connect(socket).unwrap();
            tunnel
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut line =
                serde_json::to_vec(&RouteRequest::new(name, SUBJECT, incarnation)).unwrap();
            line.push(b'\n');
            tunnel.write_all(&line).unwrap();
            let mut answer = Vec::new();
            let mut byte = [0_u8; 1];
            while tunnel.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                answer.push(byte[0]);
            }
            (tunnel, String::from_utf8(answer).unwrap())
        }

        /// Route to `name`, attach, and wait for its screen. Returns the tunnel and how long the
        /// screen took from the first byte sent.
        fn attach(socket: &Path, name: &str) -> (UnixStream, Duration) {
            let started = Instant::now();
            let (mut tunnel, answer) = route_to(socket, name, &incarnation());
            assert_eq!(answer, r#"{"ok":true}"#);
            tunnel
                .write_all(&pty_core::protocol::encode_attach(24, 80))
                .unwrap();
            let mut reader = PacketReader::new();
            let mut bytes = [0_u8; 4096];
            loop {
                let count = tunnel.read(&mut bytes).expect("the screen arrives");
                assert!(count > 0, "the tunnel closed before the screen");
                if reader
                    .feed(&bytes[..count])
                    .unwrap()
                    .iter()
                    .any(|packet| packet.type_ == MessageType::Screen)
                {
                    return (tunnel, started.elapsed());
                }
            }
        }

        /// Detach and wait for the owner to close the tunnel.
        fn detach(mut tunnel: UnixStream) {
            tunnel
                .write_all(&pty_core::protocol::encode_detach())
                .unwrap();
            let mut bytes = [0_u8; 4096];
            while tunnel
                .read(&mut bytes)
                .expect("the owner closes the tunnel")
                > 0
            {}
        }

        fn eventually(what: &str, done: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "{what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn released(service: &TerminalSocket, sessions: &[&Session]) {
            eventually("every tunnel ends", || service.live() == 0);
            for session in sessions {
                eventually("every session connection closes", || {
                    session.open.load(Ordering::SeqCst) == 0
                });
            }
        }

        #[test]
        fn a_tunnel_attaches_detaches_and_attaches_again() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            for _ in 0..3 {
                let (tunnel, _) = attach(&socket, "example-worker");
                detach(tunnel);
            }
            assert_eq!(worker.attaches.load(Ordering::SeqCst), 3);
            released(&service, &[&worker]);
        }

        #[test]
        fn another_incarnation_is_refused_and_its_session_receives_nothing() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            let stale = format!("{}:2026-09-29T07:00:00.000Z", std::process::id());
            let (mut tunnel, answer) = route_to(&socket, "example-worker", &stale);
            assert!(answer.contains("changed incarnation"), "{answer}");
            assert_eq!(tunnel.read(&mut [0_u8; 1]).unwrap(), 0, "the owner closes");
            let (_, missing) = route_to(&socket, "example-missing", &incarnation());
            assert!(missing.contains("no PTY session"), "{missing}");
            assert_eq!(worker.attaches.load(Ordering::SeqCst), 0);
            released(&service, &[&worker]);
        }

        #[test]
        fn many_tunnels_attach_at_once() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            let tunnels: Vec<_> = (0..32)
                .map(|_| {
                    let socket = socket.clone();
                    std::thread::spawn(move || attach(&socket, "example-worker").0)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|attaching| attaching.join().unwrap())
                .collect();
            assert_eq!(worker.attaches.load(Ordering::SeqCst), 32);
            assert_eq!(service.live(), 32);
            for tunnel in tunnels {
                detach(tunnel);
            }
            released(&service, &[&worker]);
        }

        #[test]
        fn an_idle_tunnel_delays_no_other_attach() {
            let (root, socket, service) = service(Duration::from_millis(500));
            let worker = session(&root.path().join("pty"), "example-worker", false);
            // One tunnel never sends its route line, and one attaches and then sends nothing.
            let mut silent = UnixStream::connect(&socket).unwrap();
            let (quiet, _) = attach(&socket, "example-worker");
            let (other, took) = attach(&socket, "example-worker");
            assert!(took < Duration::from_secs(1), "attaching took {took:?}");
            silent
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_eq!(
                silent.read(&mut [0_u8; 1]).unwrap(),
                0,
                "a tunnel with no route line is closed after the route timeout"
            );
            detach(quiet);
            detach(other);
            released(&service, &[&worker]);
        }

        #[test]
        fn a_tunnel_that_stops_reading_delays_no_other_attach() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let busy = session(&root.path().join("pty"), "example-busy", true);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            // This attach reads its screen and then nothing, while its session keeps writing.
            let (slow, _) = attach(&socket, "example-busy");
            std::thread::sleep(Duration::from_millis(200));
            for _ in 0..5 {
                let (tunnel, took) = attach(&socket, "example-worker");
                assert!(took < Duration::from_secs(1), "attaching took {took:?}");
                detach(tunnel);
            }
            drop(slow);
            released(&service, &[&busy, &worker]);
        }

        #[test]
        fn a_tunnel_that_disappears_releases_its_session() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            // Gone halfway through its route line.
            let mut partial = UnixStream::connect(&socket).unwrap();
            partial.write_all(br#"{"op":"route","#).unwrap();
            drop(partial);
            // Gone while attached, with no DETACH.
            let (attached, _) = attach(&socket, "example-worker");
            drop(attached);
            // Gone after its route was answered, before it attached.
            let (answered, answer) = route_to(&socket, "example-worker", &incarnation());
            assert_eq!(answer, r#"{"ok":true}"#);
            drop(answered);
            released(&service, &[&worker]);
            let (tunnel, _) = attach(&socket, "example-worker");
            detach(tunnel);
            released(&service, &[&worker]);
        }

        #[test]
        fn repeated_attach_cycles_leave_nothing_open() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            for _ in 0..200 {
                let (tunnel, _) = attach(&socket, "example-worker");
                detach(tunnel);
            }
            assert_eq!(worker.attaches.load(Ordering::SeqCst), 200);
            released(&service, &[&worker]);
        }

        /// The service runs on its own thread and runtime, so a daemon whose runtime is busy
        /// still answers attaches.
        #[tokio::test(flavor = "current_thread")]
        async fn the_service_answers_while_the_runtime_that_started_it_is_blocked() {
            let (root, socket, service) = service(ROUTE_TIMEOUT);
            let worker = session(&root.path().join("pty"), "example-worker", false);
            // This blocks the only thread of the runtime that started the service.
            let (tunnel, took) = attach(&socket, "example-worker");
            assert!(took < Duration::from_secs(1), "attaching took {took:?}");
            detach(tunnel);
            released(&service, &[&worker]);
        }

        #[test]
        fn the_socket_and_its_directory_are_this_accounts_alone() {
            use std::os::unix::fs::PermissionsExt as _;
            let (_root, socket, _service) = service(ROUTE_TIMEOUT);
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&socket), 0o600);
            assert_eq!(mode(socket.parent().unwrap()), 0o700);
        }

        #[test]
        fn a_second_service_never_replaces_a_live_one() {
            let (root, socket, _service) = service(ROUTE_TIMEOUT);
            let second = TerminalSocket::start(&socket, root.path().join("pty"));
            assert!(
                format!("{:#}", second.err().unwrap()).contains("refusing to replace"),
                "the live listener stays"
            );
            // A socket file left by a daemon that is gone is replaced.
            let stale = root.path().join("run/stale.sock");
            drop(UnixListener::bind(&stale).unwrap());
            TerminalSocket::start(&stale, root.path().join("pty")).unwrap();
        }
    }
}
