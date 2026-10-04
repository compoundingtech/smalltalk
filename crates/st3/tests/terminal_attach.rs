#![cfg(unix)]
//! `st terminals attach` reaches a terminal on this host through its PTY session, and a terminal
//! behind an HTTP endpoint through the daemon's WebSocket. When the daemon, at whichever endpoint,
//! is down or does not answer within a second, it attaches to the subject's newest PTY session on
//! this host without st. A terminal another host owns is attached PTY to PTY over Fabric, through
//! `st terminals serve-fabric` on the owner, and through the client gateway only when Fabric
//! cannot reach it. Each test owns a daemon, in process or stand-in, and a stand-in PTY session
//! that reports which process attached to it.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::api::AppState;
use st3::model::ClaimInput;
use st3::store::Store;
use tokio::sync::{Notify, watch};

const SUBJECT: &str = "agent/example/worker";
const RUNTIME_ID: &str = "example-worker";
const CREATED_AT: &str = "2026-09-29T08:00:00.000Z";
const SCREEN: &[u8] = b"the worker's screen";

fn state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("attach-node").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "attach-node".into(),
        state_dir: root.join("daemon"),
        pty_root: root.join("pty"),
        pty_binary: root.join("bin/pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

/// The reconciler's observation of the running terminal.
fn observe_terminal(store: &Store, incarnation: &str) {
    store
        .append_claim(&ClaimInput {
            subject: SUBJECT.into(),
            kind: "runtime.observed".into(),
            actor: Some(SUBJECT.into()),
            fields: serde_json::from_value::<BTreeMap<String, Value>>(json!({
                "runtime_id": RUNTIME_ID,
                "incarnation_id": incarnation,
                "status": "running",
                "terminal": true,
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

/// Replicate `source`, another host, into `target` until both hold the same authority.
fn replicate(source: &Store, target: &Store) {
    const FLEET: &str = "attach-fleet";
    source.bind_fleet(FLEET).unwrap();
    target.bind_fleet(FLEET).unwrap();
    for _ in 0..100 {
        let inventory = target.replication_inventory().unwrap();
        if inventory.digest == source.replication_inventory().unwrap().digest {
            return;
        }
        let exchange = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange("owner-node", FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        target.project_replication_backlog().unwrap();
    }
    panic!("the replica never converged");
}

/// The incarnation st derives for the stand-in session, which this test process serves.
fn incarnation(created_at: &str) -> String {
    format!("{}:{created_at}", std::process::id())
}

/// One client connection the stand-in PTY session accepted.
struct Attached {
    /// The connecting process, when it was still alive to be identified.
    pid: Option<i32>,
    received: Vec<u8>,
}

/// A stand-in `pty` session: a registry record and a socket under `ROOT/pty`. It answers ATTACH
/// with a screen and an exit, as a `pty` daemon does, and gives up after ten seconds without a
/// client.
fn pty_session(root: &Path) -> std::thread::JoinHandle<Option<Attached>> {
    serve_pty_session(
        &root.join("pty"),
        RUNTIME_ID,
        json!({ "createdAt": CREATED_AT }),
    )
}

/// A stand-in `pty` session `runtime_id` under `pty_root` with registry record `metadata`.
fn serve_pty_session(
    pty_root: &Path,
    runtime_id: &str,
    metadata: Value,
) -> std::thread::JoinHandle<Option<Attached>> {
    std::fs::create_dir_all(pty_root).unwrap();
    std::fs::write(
        pty_root.join(format!("{runtime_id}.json")),
        metadata.to_string(),
    )
    .unwrap();
    let listener =
        std::os::unix::net::UnixListener::bind(pty_root.join(format!("{runtime_id}.sock")))
            .unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept a PTY client: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        let pid = pty_core::unix_peer::credentials(&stream).map(|peer| peer.pid);
        let mut received = Vec::new();
        let mut reader = pty_core::protocol::PacketReader::new();
        let mut bytes = [0_u8; 4096];
        loop {
            let count = stream.read(&mut bytes).unwrap_or(0);
            if count == 0 {
                return Some(Attached { pid, received });
            }
            received.extend_from_slice(&bytes[..count]);
            if reader
                .feed(&bytes[..count])
                .unwrap()
                .iter()
                .any(|packet| packet.type_ == pty_core::protocol::MessageType::Attach)
            {
                stream
                    .write_all(&pty_core::protocol::encode_screen(SCREEN))
                    .unwrap();
                stream
                    .write_all(&pty_core::protocol::encode_exit(0))
                    .unwrap();
            }
        }
    })
}

/// The configured daemon's PTY registry for `root`, as `configured_attach` sees it: `running`
/// sessions name this test process as their PTY daemon.
struct ConfiguredRegistry {
    pty_root: PathBuf,
}

impl ConfiguredRegistry {
    fn new(root: &Path) -> Self {
        let pty_root = root.join("state/st3/pty");
        std::fs::create_dir_all(&pty_root).unwrap();
        Self { pty_root }
    }

    fn record(&self, runtime_id: &str, metadata: Value) {
        std::fs::write(
            self.pty_root.join(format!("{runtime_id}.json")),
            metadata.to_string(),
        )
        .unwrap();
    }

    fn running(&self, runtime_id: &str) {
        std::fs::write(
            self.pty_root.join(format!("{runtime_id}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
    }

    /// A running stand-in session of `SUBJECT` that this process serves.
    fn serve(
        &self,
        runtime_id: &str,
        created_at: &str,
    ) -> std::thread::JoinHandle<Option<Attached>> {
        let session = serve_pty_session(
            &self.pty_root,
            runtime_id,
            json!({ "createdAt": created_at, "tags": { "st3.subject": SUBJECT } }),
        );
        self.running(runtime_id);
        session
    }
}

/// A `pty` on the CLI's PATH that records any run: attaching must never start a session.
fn recording_pty(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pty = bin.join("pty");
    std::fs::write(
        &pty,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n",
            root.join("pty-runs").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Run `st terminals attach` against `endpoint` with no terminal on stdin. This host's PTY root
/// is `ROOT/state/st3/pty`, as `ConfiguredRegistry` writes it.
async fn attach(root: &Path, endpoint: &str) -> (Output, u32) {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let mut command = st3::test_support::command(binary);
    // An operator command: the harness running the suite must not lend its seat identity, and
    // its own PTY session must not trip the nested-attach guard.
    command
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("PATH", recording_pty(root))
        .args(["--endpoint", endpoint, "terminals", "attach", SUBJECT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let pid = child.id();
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    (output, pid)
}

/// Run `st terminals attach` against the configured daemon for `root`: its socket is
/// `ROOT/run/st3.sock` and its PTY root `ROOT/state/st3/pty`. Returns the output, the CLI's pid,
/// and how long it ran.
async fn configured_attach(root: &Path, daemon_wait: &str) -> (Output, u32, Duration) {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let mut command = st3::test_support::command(binary);
    command
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env_remove("ST3_DAEMON_WAIT")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("PATH", recording_pty(root))
        .args(["--daemon-wait", daemon_wait, "terminals", "attach", SUBJECT])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let child = command.spawn().unwrap();
    let pid = child.id();
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    (output, pid, started.elapsed())
}

async fn serve_unix(state: AppState, socket: &Path) -> tokio::task::JoinHandle<()> {
    let server_socket = socket.to_path_buf();
    let server = tokio::spawn(async move {
        let _ = st3::api::serve_unix(&server_socket, st3::api::router(state)).await;
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "the daemon never listened");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    server
}

fn assert_attached(output: &Output) {
    assert!(
        output.status.success(),
        "attach failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(SCREEN.len())
            .any(|window| window == SCREEN),
        "the terminal's screen was not shown: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_this_host_attaches_through_its_pty_session() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    observe_terminal(&state.store, &incarnation(CREATED_AT));
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    let session = pty_session(root.path());

    let (output, cli) = attach(root.path(), socket.to_str().unwrap()).await;

    assert_attached(&output);
    let attached = session.join().unwrap().expect("the CLI attached");
    assert_eq!(
        attached.pid,
        Some(cli as i32),
        "the CLI itself must hold the PTY connection, not the daemon"
    );
    assert!(
        !root.path().join("pty-runs").exists(),
        "attaching ran `pty`, which can start the session again"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_behind_an_http_endpoint_attaches_through_the_websocket() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    observe_terminal(&state.store, &incarnation(CREATED_AT));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, st3::api::router(state)).await;
    });
    let session = pty_session(root.path());

    let (output, _) = attach(root.path(), &format!("http://{address}")).await;

    assert_attached(&output);
    let attached = session.join().unwrap().expect("the daemon attached");
    assert_eq!(
        attached.pid,
        Some(std::process::id() as i32),
        "the daemon's WebSocket bridge must hold the PTY connection"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_pty_session_receives_nothing_from_a_local_attach() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    // st observed an earlier session; a replacement now serves the same socket path.
    observe_terminal(&state.store, &incarnation("2026-09-29T07:00:00.000Z"));
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    let session = pty_session(root.path());

    let (output, _) = attach(root.path(), socket.to_str().unwrap()).await;

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("changed incarnation"), "{stderr}");
    let attached = session.join().unwrap().expect("the CLI connected to check");
    assert!(
        attached.received.is_empty(),
        "a fenced-out session must receive nothing: {:?}",
        attached.received
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_this_host_attaches_without_st_while_its_daemon_is_down() {
    let root = tempfile::tempdir().unwrap();
    // The configured daemon's socket under XDG_RUNTIME_DIR never listens.
    let registry = ConfiguredRegistry::new(root.path());
    let session = registry.serve(RUNTIME_ID, CREATED_AT);
    // An earlier session of the subject that still runs; the newest is attached.
    registry.record(
        "example-worker-earlier",
        json!({ "createdAt": "2026-09-29T07:00:00.000Z", "tags": { "st3.subject": SUBJECT } }),
    );
    registry.running("example-worker-earlier");
    registry.record(
        "example-other",
        json!({ "createdAt": CREATED_AT, "tags": { "st3.subject": "agent/example/other" } }),
    );
    registry.running("example-other");
    // A later session of the subject whose daemon wrote its exit record and left.
    registry.record(
        "example-worker-exited",
        json!({
            "createdAt": "2026-09-29T09:00:00.000Z",
            "exitedAt": "2026-09-29T09:30:00.000Z",
            "exitCode": 0,
            "tags": { "st3.subject": SUBJECT },
        }),
    );

    let (output, cli, _) = configured_attach(root.path(), "0").await;

    assert_attached(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("st was not consulted") && stderr.contains(&format!("`{RUNTIME_ID}`")),
        "the attachment must say st was not consulted: {stderr}"
    );
    assert!(
        stderr.contains("also running: `example-worker-earlier`"),
        "the subject's other running session is named: {stderr}"
    );
    assert!(
        !stderr.contains("example-other") && !stderr.contains("example-worker-exited"),
        "only the subject's running sessions are named: {stderr}"
    );
    let attached = session.join().unwrap().expect("the CLI attached");
    assert_eq!(attached.pid, Some(cli as i32));
    assert!(
        !root.path().join("pty-runs").exists(),
        "attaching ran `pty`, which can start the session again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_this_host_attaches_while_its_daemon_never_answers() {
    let root = tempfile::tempdir().unwrap();
    // The configured daemon takes every connection and never answers, as a daemon stuck behind
    // a saturated disk does.
    let run = root.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let silent = SilentDaemon::unix(&run.join("st3.sock"));
    let registry = ConfiguredRegistry::new(root.path());
    let session = registry.serve(RUNTIME_ID, CREATED_AT);

    let (output, cli, elapsed) = configured_attach(root.path(), "30").await;

    assert_attached(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did not answer within 1000 ms") && stderr.contains("st was not consulted"),
        "{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the attach waited {elapsed:?} for a daemon that never answers"
    );
    let attached = session.join().unwrap().expect("the CLI attached");
    assert_eq!(attached.pid, Some(cli as i32));
    assert!(silent.stop() > 0, "st must still ask its daemon first");
}

/// A daemon that takes every connection and never answers.
struct SilentDaemon {
    stop: Arc<std::sync::atomic::AtomicBool>,
    accepting: std::thread::JoinHandle<usize>,
}

impl SilentDaemon {
    fn unix(socket: &Path) -> Self {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self::serve(move || {
            listener
                .accept()
                .map(|(stream, _)| Box::new(stream) as Box<dyn Send>)
        })
    }

    /// One on a loopback TCP port, with its HTTP endpoint.
    fn http() -> (Self, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let daemon = Self::serve(move || {
            listener
                .accept()
                .map(|(stream, _)| Box::new(stream) as Box<dyn Send>)
        });
        (daemon, endpoint)
    }

    fn serve(mut accept: impl FnMut() -> std::io::Result<Box<dyn Send>> + Send + 'static) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let accepting = std::thread::spawn(move || {
            let mut held = Vec::new();
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                match accept() {
                    Ok(stream) => held.push(stream),
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
            held.len()
        });
        Self { stop, accepting }
    }

    /// Stop accepting; returns how many connections it took.
    fn stop(self) -> usize {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.accepting.join().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_on_this_host_attaches_whatever_endpoint_never_answers() {
    let root = tempfile::tempdir().unwrap();
    // An explicit endpoint, not the configured daemon's socket, that never answers.
    let socket = root.path().join("elsewhere.sock");
    let unix = SilentDaemon::unix(&socket);
    let (http, http_endpoint) = SilentDaemon::http();
    let registry = ConfiguredRegistry::new(root.path());

    for (daemon, endpoint) in [(unix, socket.display().to_string()), (http, http_endpoint)] {
        let session = registry.serve(RUNTIME_ID, CREATED_AT);
        let started = Instant::now();

        let (output, cli) = attach(root.path(), &endpoint).await;

        assert_attached(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "st daemon at {endpoint} did not answer within 1000 ms"
            )) && stderr.contains("st was not consulted"),
            "{stderr}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the attach waited {:?} for {endpoint}, which never answers",
            started.elapsed()
        );
        let attached = session.join().unwrap().expect("the CLI attached");
        assert_eq!(attached.pid, Some(cli as i32));
        assert!(daemon.stop() > 0, "st must still ask {endpoint} first");
        std::fs::remove_file(registry.pty_root.join(format!("{RUNTIME_ID}.sock"))).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiting_on_a_daemon_that_does_not_answer_says_so() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("elsewhere.sock");
    let daemon = SilentDaemon::unix(&socket);
    // No PTY session of the subject runs on this host, so only the daemon can attach it.
    ConfiguredRegistry::new(root.path());
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let mut child = st3::test_support::command(binary)
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("PATH", recording_pty(root.path()))
        .args([
            "--endpoint",
            socket.to_str().unwrap(),
            "terminals",
            "attach",
            SUBJECT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let expected = format!(
        "waiting for the st daemon at {}; no PTY session of `{SUBJECT}` runs under",
        socket.display()
    );
    let reading = std::thread::spawn(move || {
        let mut seen = String::new();
        let mut bytes = [0_u8; 1024];
        while !seen.contains(&expected) {
            match stderr.read(&mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(count) => seen.push_str(&String::from_utf8_lossy(&bytes[..count])),
            }
        }
        seen
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reading.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let _ = child.wait();
    let seen = reading.join().unwrap();

    assert!(
        seen.contains("waiting for the st daemon at") && seen.contains("no PTY session of"),
        "the attach must say what it is waiting for: {seen:?}"
    );
    assert!(daemon.stop() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_another_host_owns_is_not_attached_through_a_local_pty() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    // Another host runs the terminal; this daemon knows it only through replication.
    let owner = Store::open_memory("owner-node").unwrap();
    observe_terminal(&owner, &incarnation(CREATED_AT));
    replicate(&owner, &state.store);
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    // A local session under the same name must not be mistaken for it.
    let session = pty_session(root.path());

    let (output, _) = attach(root.path(), socket.to_str().unwrap()).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    // It goes to the client gateway as a person, and none is configured here.
    assert!(stderr.contains("owner-node"), "{stderr}");
    assert!(stderr.contains("needs `--as person/NAME`"), "{stderr}");
    assert!(
        session.join().unwrap().is_none(),
        "a terminal on another host must not be attached through a local PTY"
    );
    server.abort();
}

/// The fleet the owner and this daemon share; the owner serves its PTY sessions as
/// `st3/pty/FLEET`.
const FLEET_ID: &str = "0f1e2d3c-4b5a-4968-8778-695a4b3c2d1e";
const OWNER_NODE: &str = "fabric-owner-node";

/// A host that owns the terminal, as this daemon sees it through replication, with this
/// daemon's config naming the fleet.
fn remote_terminal(root: &Path, incarnation: &str) -> AppState {
    let mut state = state(root);
    state.fleet_id = Some(FLEET_ID.into());
    let owner = Store::open_memory("owner-node").unwrap();
    observe_terminal(&owner, incarnation);
    replicate(&owner, &state.store);
    let config = root.join("config/st3");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.toml"),
        format!("fleet_id = \"{FLEET_ID}\"\n"),
    )
    .unwrap();
    state
}

/// A `fabric` on the CLI's PATH that records each call and knows the owner by name. `dial`
/// prints `tunnel`, or fails as an unreachable peer does when `tunnel` is `None`.
fn fabric_shim(root: &Path, tunnel: Option<&Path>) {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let dial = match tunnel {
        Some(tunnel) => format!("echo '{}'", tunnel.display()),
        None => "echo 'peer unreachable' >&2; exit 2".into(),
    };
    let fabric = bin.join("fabric");
    std::fs::write(
        &fabric,
        format!(
            "#!/bin/sh\necho \"$@\" >> '{calls}'\ncase \"$1\" in\n  \
             peers) printf '{OWNER_NODE}\\towner-node\\tst3/pty/{FLEET_ID}\\n';;\n  \
             dial) {dial};;\n  *) exit 1;;\nesac\n",
            calls = root.join("fabric-calls").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fabric, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The owner's end of a Fabric tunnel at `socket`: for the first tunnel, run
/// `st terminals serve-fabric` over the owner's PTY root with the tunnel on its stdin and stdout,
/// as Fabric's exec exposure does. Returns that process's pid.
fn fabric_tunnel(socket: &Path, owner_pty_root: &Path) -> std::thread::JoinHandle<Option<u32>> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let pty_root = owner_pty_root.to_path_buf();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let tunnel = loop {
            match listener.accept() {
                Ok((tunnel, _)) => break tunnel,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept a Fabric tunnel: {error}"),
            }
        };
        tunnel.set_nonblocking(false).unwrap();
        let mut serve = st3::test_support::command(binary)
            .env_remove("ST_AGENT")
            .env_remove("PTY_SESSION")
            .args(["terminals", "serve-fabric", "--stdio", "--pty-root"])
            .arg(&pty_root)
            .stdin(std::os::fd::OwnedFd::from(tunnel.try_clone().unwrap()))
            .stdout(std::os::fd::OwnedFd::from(tunnel))
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let pid = serve.id();
        serve.wait().unwrap();
        Some(pid)
    })
}

/// Run `st terminals attach` as a person against `endpoint`, with this test's config and state.
async fn remote_attach(root: &Path, endpoint: &str) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let mut command = st3::test_support::command(binary);
    command
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("PATH", recording_pty(root))
        .args(["--endpoint", endpoint, "terminals", "attach", SUBJECT])
        .args(["--as", "person/example"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap()
}

/// The owner's stand-in session of `SUBJECT`, started at `created_at`, under `ROOT/owner-pty`.
fn owner_session(root: &Path, created_at: &str) -> std::thread::JoinHandle<Option<Attached>> {
    serve_pty_session(
        &root.join("owner-pty"),
        RUNTIME_ID,
        json!({ "createdAt": created_at, "tags": { "st3.subject": SUBJECT } }),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_another_host_owns_attaches_pty_to_pty_over_fabric() {
    let root = tempfile::tempdir().unwrap();
    let state = remote_terminal(root.path(), &incarnation(CREATED_AT));
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    let tunnel = root.path().join("tunnel.sock");
    fabric_shim(root.path(), Some(&tunnel));
    let owner = fabric_tunnel(&tunnel, &root.path().join("owner-pty"));
    let session = owner_session(root.path(), CREATED_AT);

    let output = remote_attach(root.path(), socket.to_str().unwrap()).await;

    assert_attached(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("client gateway"), "{stderr}");
    let served = owner.join().unwrap().expect("the CLI dialed the owner");
    let attached = session.join().unwrap().expect("the owner attached");
    assert_eq!(
        attached.pid,
        Some(served as i32),
        "the owner's `st terminals serve-fabric` must hold the PTY connection"
    );
    let calls = std::fs::read_to_string(root.path().join("fabric-calls")).unwrap();
    assert!(
        calls.contains(&format!("dial {OWNER_NODE} st3/pty/{FLEET_ID}")),
        "{calls}"
    );
    assert!(
        !root.path().join("pty-runs").exists(),
        "attaching ran `pty`, which can start the session again"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replaced_pty_session_on_another_host_receives_nothing_over_fabric() {
    let root = tempfile::tempdir().unwrap();
    // st selected an earlier session; a replacement now serves the same name on the owner.
    let state = remote_terminal(root.path(), &incarnation("2026-09-29T07:00:00.000Z"));
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    let tunnel = root.path().join("tunnel.sock");
    fabric_shim(root.path(), Some(&tunnel));
    let owner = fabric_tunnel(&tunnel, &root.path().join("owner-pty"));
    let session = owner_session(root.path(), CREATED_AT);

    let output = remote_attach(root.path(), socket.to_str().unwrap()).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("changed incarnation")
            && stderr.contains("Attaching through the client gateway instead"),
        "the owner refuses the route, and st falls back: {stderr}"
    );
    assert!(owner.join().unwrap().is_some(), "the CLI dialed the owner");
    let attached = session
        .join()
        .unwrap()
        .expect("the owner connected to check");
    assert!(
        attached.received.is_empty(),
        "a fenced-out session must receive nothing: {:?}",
        attached.received
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_another_host_owns_falls_back_to_the_client_gateway_without_fabric() {
    let root = tempfile::tempdir().unwrap();
    let state = remote_terminal(root.path(), &incarnation(CREATED_AT));
    let socket = root.path().join("st3.sock");
    let server = serve_unix(state, &socket).await;
    fabric_shim(root.path(), None);
    let session = owner_session(root.path(), CREATED_AT);

    let output = remote_attach(root.path(), socket.to_str().unwrap()).await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("peer unreachable")
            && stderr.contains("Attaching through the client gateway instead"),
        "{stderr}"
    );
    // This daemon has no relay to the owner, so the gateway cannot reach it either.
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("owner-node"), "{stderr}");
    let calls = std::fs::read_to_string(root.path().join("fabric-calls")).unwrap();
    assert!(
        calls.contains(&format!("dial {OWNER_NODE} st3/pty/{FLEET_ID}")),
        "{calls}"
    );
    assert!(
        session.join().unwrap().is_none(),
        "nothing reached the owner's session"
    );
    server.abort();
}
