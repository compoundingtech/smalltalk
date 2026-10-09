//! A terminal's PTY session reached without an st daemon carrying its bytes: the local socket for
//! a terminal on this host, Fabric to the owner for one on another fleet host. The local daemon
//! only says where a terminal lives (a read of its own store); the paired-device gateway remains
//! only for a device that has no daemon and no fleet of its own, such as the phone.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use st3_client::Client;
use st3_terminal_direct::{FabricTarget, LocalTerminal, RouteError, RouteRequest};

/// How a terminal's stream was opened, and so what a failure means.
pub(super) enum Opened {
    /// The PTY session's stream, and the session and incarnation it proved.
    Stream {
        stream: UnixStream,
        name: String,
        incarnation: String,
    },
    /// This device has no daemon of its own to say where a terminal lives, so it asks the
    /// gateway it is paired with.
    UseGateway,
}

/// What `open` was told about the terminal. A restarted terminal is never quietly swapped in.
pub(super) struct Terminal<'a> {
    /// The agent (`agent/...`) or shell (`terminal/...`) subject: the PTY session's `st3.subject`.
    pub subject: &'a str,
    /// The PTY session's name, when st already said it.
    pub name: &'a str,
    /// The running incarnation, when st already said it.
    pub incarnation: &'a str,
    /// The fleet host that owns the terminal, when st already said it (`host/NAME` or `NAME`).
    pub owner: Option<&'a str>,
    /// Only this incarnation, as a reattach needs.
    pub expected: Option<&'a str>,
}

pub(super) const RESTARTED: &str = "the terminal restarted; Ctrl+] attaches the new one";

pub(super) async fn open(client: &Client, terminal: &Terminal<'_>) -> Result<Opened, String> {
    open_in(client, terminal, state_dir().as_deref()).await
}

/// `open`, with the directory that holds this machine's fleet file.
async fn open_in(
    client: &Client,
    terminal: &Terminal<'_>,
    state_dir: Option<&Path>,
) -> Result<Opened, String> {
    if !client.is_local_socket() {
        return Ok(Opened::UseGateway);
    }
    match client.local_terminal(terminal.subject).await {
        Ok(Some(here)) => {
            if terminal
                .expected
                .is_some_and(|expected| expected != here.incarnation_id)
            {
                return Err(RESTARTED.into());
            }
            let local = LocalTerminal {
                subject: here.subject,
                runtime_id: here.runtime_id,
                incarnation_id: here.incarnation_id,
                pty_root: here.pty_root,
            };
            let (name, incarnation) = (local.runtime_id.clone(), local.incarnation_id.clone());
            st3_terminal_direct::open_local_terminal(&local)
                .await
                .map(|stream| Opened::Stream {
                    stream,
                    name,
                    incarnation,
                })
                .map_err(|error| format!("{error:#}"))
        }
        Ok(None) => over_fabric(client, terminal, state_dir)
            .await
            .map(|stream| Opened::Stream {
                stream,
                name: terminal.name.to_owned(),
                incarnation: terminal.incarnation.to_owned(),
            }),
        Err(error) => Err(error.plain()),
    }
}

async fn over_fabric(
    client: &Client,
    terminal: &Terminal<'_>,
    state_dir: Option<&Path>,
) -> Result<UnixStream, String> {
    let owner = terminal
        .owner
        .map(|owner| owner.trim_start_matches("host/"))
        .filter(|owner| !owner.is_empty())
        .ok_or_else(|| {
            format!(
                "st does not say which host runs `{}`, so there is nowhere to attach directly",
                terminal.subject
            )
        })?;
    if terminal.name.is_empty() || terminal.incarnation.is_empty() {
        return Err(format!(
            "st has no running terminal for `{}` on {owner}",
            terminal.subject
        ));
    }
    // The owner proves the terminal is the one st selected but not who may read it, so the
    // person's grant is checked here, as `st terminals attach` checks it, before any dial.
    let capabilities = client.capabilities().await.map_err(|error| error.plain())?;
    for scope in ["terminal.read", "terminal.control"] {
        if !capabilities.value.capabilities.iter().any(|capability| {
            capability.id == scope && capability.state == st3_client::CapabilityState::Granted
        }) {
            return Err(format!(
                "st does not grant you `{scope}`, so {owner}'s terminal is not attached directly"
            ));
        }
    }
    let (fabric, fleet_id) = fleet_fabric(owner, state_dir)?;
    // The advertised node first, as the CLI does; the trusted Fabric peer of that name otherwise.
    let members = match client.fleet_membership().await {
        Ok(value) => serde_json::from_value::<smallclaims::fleet::view::FleetView>(value)
            .map(|view| view.members)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let peer = st3_terminal_direct::owner_peer(&fabric, &members, owner)
        .await
        .ok_or_else(|| {
            format!(
                "{owner} advertises no Fabric node, and no Fabric peer of this machine has its name"
            )
        })?;
    let protocol = st3_terminal_direct::protocol(&fleet_id);
    let target = FabricTarget {
        fabric,
        peer,
        protocol: protocol.clone(),
    };
    let request = RouteRequest::new(terminal.name, terminal.subject, terminal.incarnation);
    st3_terminal_direct::open_route(&target, &request)
        .await
        .map_err(|error| match error {
            RouteError::Refused(reason) => format!("{owner} refused the terminal: {reason}"),
            RouteError::Unreachable(reason) => format!(
                "Fabric did not reach {owner}'s terminals: {reason}. `fabric probe {owner} {protocol}` says whether the peer is up and grants this machine; `st terminals expose-fabric` on {owner} serves it"
            ),
        })
}

/// This machine's `fabric` and fleet: the fleet file's override or the one on `PATH`.
fn fleet_fabric(
    owner: &str,
    state_dir: Option<&Path>,
) -> Result<(smallclaims::fleet::transport::Fabric, String), String> {
    let state_dir = state_dir.ok_or("this machine has no state directory to find its fleet in")?;
    let file = smallclaims::fleet::file::FleetFile::load(state_dir)
        .map_err(|error| format!("{error:#}"))?
        .ok_or_else(|| format!("this machine is in no fleet to reach {owner} through"))?;
    let program = smallclaims::fleet::transport::resolve_tool(file.fabric.as_deref(), "fabric")
        .ok_or_else(|| format!("this machine has no `fabric` to reach {owner} with"))?;
    Ok((
        smallclaims::fleet::transport::Fabric::new(program),
        file.fleet_id,
    ))
}

fn state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("st3"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use st3::model::ClaimInput;
    use std::collections::BTreeMap;
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SUBJECT: &str = "agent/example/worker";
    const RUNTIME_ID: &str = "example-worker";
    const CREATED_AT: &str = "2026-09-29T08:00:00.000Z";
    const FLEET_ID: &str = "0f1e2d3c-4b5a-4968-8778-695a4b3c2d1e";
    const OWNER_NODE: &str = "fabric-owner-node";

    fn incarnation(created_at: &str) -> String {
        format!("{}:{created_at}", std::process::id())
    }

    fn state(root: &Path) -> st3::api::AppState {
        st3::api::AppState {
            store: Arc::new(st3::store::Store::open_memory("attach-node").unwrap()),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0_u64).0,
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

    fn observe_terminal(store: &st3::store::Store, incarnation: &str) {
        store
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: "runtime.observed".into(),
                actor: Some(SUBJECT.into()),
                fields: serde_json::from_value::<BTreeMap<String, serde_json::Value>>(json!({
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

    fn replicate(source: &st3::store::Store, target: &st3::store::Store) {
        source.bind_fleet("attach-fleet").unwrap();
        target.bind_fleet("attach-fleet").unwrap();
        for _ in 0..100 {
            let inventory = target.replication_inventory().unwrap();
            if inventory.digest == source.replication_inventory().unwrap().digest {
                return;
            }
            let exchange = source
                .export_replication_exchange("attach-fleet", &inventory)
                .unwrap();
            target
                .receive_replication_exchange("owner-node", "attach-fleet", &exchange)
                .unwrap();
            target.validate_replication_backlog().unwrap();
            target.apply_replication_repairs().unwrap();
            target.project_replication_backlog().unwrap();
        }
        panic!("the replica never converged");
    }

    /// A stand-in PTY session: a registry record and a socket under `pty_root`. It reports which
    /// process connected, and gives up after ten seconds without a client.
    fn pty_session(
        pty_root: &Path,
        tagged: bool,
    ) -> std::thread::JoinHandle<Option<(Option<i32>, Vec<u8>)>> {
        std::fs::create_dir_all(pty_root).unwrap();
        let mut metadata = json!({ "createdAt": CREATED_AT });
        if tagged {
            metadata["tags"] = json!({ "st3.subject": SUBJECT });
        }
        std::fs::write(
            pty_root.join(format!("{RUNTIME_ID}.json")),
            metadata.to_string(),
        )
        .unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(pty_root.join(format!("{RUNTIME_ID}.sock")))
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
            let mut bytes = [0_u8; 4096];
            loop {
                let count = stream.read(&mut bytes).unwrap_or(0);
                if count == 0 {
                    return Some((pid, received));
                }
                received.extend_from_slice(&bytes[..count]);
                if received.ends_with(b"hello") {
                    stream.write_all(b"world").unwrap();
                }
            }
        })
    }

    async fn serve(state: st3::api::AppState, socket: &Path) -> tokio::task::JoinHandle<()> {
        let path = socket.to_path_buf();
        let server = tokio::spawn(async move {
            let _ = st3::api::serve_unix(&path, st3::api::router(state)).await;
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon never listened");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        server
    }

    /// Say `hello` on the stream and read the stand-in session's `world`.
    fn exchange(mut stream: UnixStream) {
        stream.write_all(b"hello").unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answer = [0_u8; 5];
        stream.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"world");
    }

    /// Time the real attach path to a live seat, read only: the runtime read stui makes, the
    /// route (local socket or Fabric), and the first screen of a PEEK, which sends no input and
    /// resizes nothing. Run by hand against a real daemon and fleet:
    /// `STUI_PROBE_SUBJECT=agent/... STUI_PROBE_PERSON=person/... cargo test -p stui
    /// live_direct_probe -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "reads a live seat through the real daemon and Fabric"]
    async fn live_direct_probe() {
        let subject = std::env::var("STUI_PROBE_SUBJECT").expect("STUI_PROBE_SUBJECT");
        let person = std::env::var("STUI_PROBE_PERSON").expect("STUI_PROBE_PERSON");
        let runtime = std::env::var("STUI_PROBE_RUNTIME").expect("STUI_PROBE_RUNTIME");
        let socket = st3_client::discover_unix_endpoint(
            std::env::var_os("ST3_ENDPOINT").map(PathBuf::from),
        )
        .unwrap();
        let client = Client::unix_as(&socket, person);
        let started = Instant::now();
        // stui's own feed already holds an agent's incarnation and host, so a real attach reads
        // no runtime. With STUI_PROBE_INCARNATION and STUI_PROBE_OWNER (from `st agents show
        // AGENT --json`: value.incarnation_id, value.host_id) the probe does the same; without
        // them it makes the runtime read, which a busy daemon can take seconds to answer.
        let (name, incarnation, owner, terminal) = match (
            std::env::var("STUI_PROBE_INCARNATION"),
            std::env::var("STUI_PROBE_OWNER"),
        ) {
            (Ok(incarnation), Ok(owner)) => (
                runtime.trim_start_matches("runtime/").to_owned(),
                incarnation,
                owner,
                format!("terminal/{subject}"),
            ),
            _ => {
                let envelope = client.runtimes_get(&runtime).await.unwrap();
                let st3_client::Resource::Runtime(found) = envelope.value else {
                    panic!("not a runtime")
                };
                (
                    found.runtime_id.clone(),
                    found.incarnation_id.clone().expect("running incarnation"),
                    found.owner_host_id.clone(),
                    found.terminal_id.clone().expect("a terminal"),
                )
            }
        };
        let read = started.elapsed();
        let opened_at = Instant::now();
        let opened = open(
            &client,
            &Terminal {
                subject: &subject,
                name: &name,
                incarnation: &incarnation,
                owner: Some(&owner),
                expected: None,
            },
        )
        .await
        .unwrap();
        let routed = opened_at.elapsed();
        let Opened::Stream { mut stream, .. } = opened else {
            panic!("gateway")
        };
        stream.write_all(&pty_core::protocol::encode_peek(true, false)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut reader = pty_core::protocol::PacketReader::new();
        let mut bytes = [0_u8; 65536];
        let mut first = None;
        while first.is_none() {
            let count = stream.read(&mut bytes).unwrap();
            assert!(count > 0, "the session closed before a screen");
            first = reader
                .feed(&bytes[..count])
                .unwrap()
                .into_iter()
                .find(|packet| packet.type_ == pty_core::protocol::MessageType::Screen);
        }
        println!(
            "PROBE direct subject={subject} owner={owner} runtime_read={read:?} route={routed:?} \
             first_screen={:?} total={:?}",
            opened_at.elapsed() - routed,
            started.elapsed()
        );
        // The same seat through the client gateway, as stui attached before: the daemon relays.
        let gateway = Instant::now();
        let mut relayed = client
            .raw_terminal_peek(&terminal, &incarnation)
            .await
            .unwrap()
            .stream;
        let opened = gateway.elapsed();
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        relayed
            .write_all(&pty_core::protocol::encode_peek(true, false))
            .await
            .unwrap();
        let mut reader = pty_core::protocol::PacketReader::new();
        let mut seen = false;
        while !seen {
            let count = relayed.read(&mut bytes).await.unwrap();
            assert!(count > 0, "the gateway closed before a screen");
            seen = reader
                .feed(&bytes[..count])
                .unwrap()
                .iter()
                .any(|packet| packet.type_ == pty_core::protocol::MessageType::Screen);
        }
        println!(
            "PROBE gateway subject={subject} open={opened:?} first_screen={:?} total={:?}",
            gateway.elapsed() - opened,
            gateway.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_on_this_host_is_its_pty_session_and_no_daemon_carries_it() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        observe_terminal(&state.store, &incarnation(CREATED_AT));
        let socket = root.path().join("st3.sock");
        let server = serve(state, &socket).await;
        let session = pty_session(&root.path().join("pty"), true);
        let client = Client::unix_as(&socket, "person/example");
        let opened = open_in(
            &client,
            &Terminal {
                subject: SUBJECT,
                name: "",
                incarnation: "",
                owner: None,
                expected: None,
            },
            None,
        )
        .await
        .unwrap();

        let Opened::Stream {
            stream,
            name,
            incarnation: proved,
        } = opened
        else {
            panic!("a local terminal must not use the gateway")
        };
        assert_eq!(
            (name.as_str(), proved),
            (RUNTIME_ID, incarnation(CREATED_AT))
        );
        exchange(stream);
        let (peer, _) = session
            .join()
            .unwrap()
            .expect("the PTY session was connected");
        assert_eq!(
            peer,
            Some(std::process::id() as i32),
            "this process holds the PTY connection"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_terminal_that_restarted_is_not_swapped_in_on_reattach() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        observe_terminal(&state.store, &incarnation(CREATED_AT));
        let socket = root.path().join("st3.sock");
        let server = serve(state, &socket).await;
        let session = pty_session(&root.path().join("pty"), true);
        let client = Client::unix_as(&socket, "person/example");

        let refused = open_in(
            &client,
            &Terminal {
                subject: SUBJECT,
                name: RUNTIME_ID,
                incarnation: "1:2026-09-28T00:00:00.000Z",
                owner: None,
                expected: Some("1:2026-09-28T00:00:00.000Z"),
            },
            None,
        )
        .await;

        assert_eq!(refused.err().as_deref(), Some(RESTARTED));
        assert!(
            session.join().unwrap().is_none(),
            "nothing may connect to a different incarnation"
        );
        server.abort();
    }

    /// A `fabric` that knows the owner by name; `dial` prints `tunnel`.
    fn fabric_shim(root: &Path, tunnel: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let fabric = root.join("fabric");
        std::fs::write(
            &fabric,
            format!(
                "#!/bin/sh\necho \"$@\" >> '{calls}'\ncase \"$1\" in\n  \
                 peers) printf '{OWNER_NODE}\\towner-node\\tst3/pty/{FLEET_ID}\\n';;\n  \
                 dial) echo '{tunnel}';;\n  *) exit 1;;\nesac\n",
                calls = root.join("fabric-calls").display(),
                tunnel = tunnel.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fabric, std::fs::Permissions::from_mode(0o755)).unwrap();
        fabric
    }

    /// The owner's end of a Fabric tunnel: read the route line, prove it names `SUBJECT` and the
    /// incarnation, answer, and splice to the owner's stand-in session.
    fn tunnel(
        socket: &Path,
        owner_pty_root: &Path,
    ) -> std::thread::JoinHandle<Option<serde_json::Value>> {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let session_socket = owner_pty_root.join(format!("{RUNTIME_ID}.sock"));
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut tunnel = loop {
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
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            while tunnel.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            let route: serde_json::Value = serde_json::from_slice(&line).unwrap();
            let mut session = UnixStream::connect(session_socket).unwrap();
            tunnel.write_all(b"{\"ok\":true}\n").unwrap();
            let (mut from_tunnel, mut to_tunnel) = (tunnel.try_clone().unwrap(), tunnel);
            let mut from_session = session.try_clone().unwrap();
            let out = std::thread::spawn(move || std::io::copy(&mut from_session, &mut to_tunnel));
            let _ = std::io::copy(&mut from_tunnel, &mut session);
            let _ = session.shutdown(std::net::Shutdown::Both);
            let _ = out.join();
            Some(route)
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_another_host_owns_is_its_pty_session_over_fabric_after_the_grant_check() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        // Another host runs the terminal; this daemon only knows it through replication.
        let owner = st3::store::Store::open_memory("owner-node").unwrap();
        observe_terminal(&owner, &incarnation(CREATED_AT));
        replicate(&owner, &state.store);
        state.fleet_id = Some(FLEET_ID.into());
        let socket = root.path().join("st3.sock");
        let server = serve(state, &socket).await;
        let tunnel_socket = root.path().join("tunnel.sock");
        let fabric = fabric_shim(root.path(), &tunnel_socket);
        let fleet_dir = root.path().join("state/st3/fleet");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        std::fs::write(
            fleet_dir.join("fleet.toml"),
            format!(
                "fleet_id = \"{FLEET_ID}\"\nsecret_file = \"secret\"\nnode_key_file = \"node.key\"\nfabric = \"{}\"\n",
                fabric.display()
            ),
        )
        .unwrap();
        let owner_pty = root.path().join("owner-pty");
        let session = pty_session(&owner_pty, true);
        let carrier = tunnel(&tunnel_socket, &owner_pty);
        let client = Client::unix_as(&socket, "person/example");
        let opened = open_in(
            &client,
            &Terminal {
                subject: SUBJECT,
                name: RUNTIME_ID,
                incarnation: &incarnation(CREATED_AT),
                owner: Some("host/owner-node"),
                expected: None,
            },
            Some(&root.path().join("state/st3")),
        )
        .await
        .unwrap();

        let Opened::Stream { stream, .. } = opened else {
            panic!("a fleet member must not use the gateway for another host's terminal")
        };
        exchange(stream);
        let route = carrier
            .join()
            .unwrap()
            .expect("the client dialed the owner");
        assert_eq!(
            route,
            json!({
                "op": "route", "name": RUNTIME_ID, "subject": SUBJECT,
                "incarnation": incarnation(CREATED_AT),
            })
        );
        session
            .join()
            .unwrap()
            .expect("the owner's session was connected");
        let calls = std::fs::read_to_string(root.path().join("fabric-calls")).unwrap();
        assert!(
            calls.contains(&format!("dial {OWNER_NODE} st3/pty/{FLEET_ID}")),
            "{calls}"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_another_host_owns_says_why_when_fabric_is_not_there() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        let owner = st3::store::Store::open_memory("owner-node").unwrap();
        observe_terminal(&owner, &incarnation(CREATED_AT));
        replicate(&owner, &state.store);
        state.fleet_id = Some(FLEET_ID.into());
        let socket = root.path().join("st3.sock");
        let server = serve(state, &socket).await;
        let client = Client::unix_as(&socket, "person/example");
        let terminal = Terminal {
            subject: SUBJECT,
            name: RUNTIME_ID,
            incarnation: &incarnation(CREATED_AT),
            owner: Some("owner-node"),
            expected: None,
        };

        let nowhere = open_in(&client, &terminal, Some(&root.path().join("no-state"))).await;
        let reason = nowhere.err().expect("no fleet, no direct path");
        assert!(reason.contains("in no fleet"), "{reason}");
        let unknown = open_in(
            &client,
            &Terminal {
                owner: None,
                ..terminal
            },
            Some(&root.path().join("no-state")),
        )
        .await;
        assert!(
            unknown
                .err()
                .expect("no owner")
                .contains("does not say which host"),
        );
        server.abort();
    }
}
