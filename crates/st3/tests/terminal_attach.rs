#![cfg(unix)]
//! `st terminals attach` reaches a terminal on this host through its PTY session, and a terminal
//! behind an HTTP endpoint through the daemon's WebSocket. Each test owns an in-process daemon and
//! a stand-in PTY session that reports which process attached to it.

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
    let pty_root = root.join("pty");
    std::fs::create_dir_all(&pty_root).unwrap();
    std::fs::write(
        pty_root.join(format!("{RUNTIME_ID}.json")),
        json!({ "createdAt": CREATED_AT }).to_string(),
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

/// Run `st terminals attach` against `endpoint` with no terminal on stdin.
async fn attach(root: &Path, endpoint: &str) -> (Output, u32) {
    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let mut command = std::process::Command::new(binary);
    // An operator command: the harness running the suite must not lend its seat identity, and
    // its own PTY session must not trip the nested-attach guard.
    command
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env("XDG_CONFIG_HOME", root.join("config"))
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
async fn a_terminal_on_this_host_is_named_but_not_attached_while_its_daemon_is_down() {
    let root = tempfile::tempdir().unwrap();
    // The configured daemon's PTY root; its socket under XDG_RUNTIME_DIR never listens.
    let pty_root = root.path().join("state/st3/pty");
    std::fs::create_dir_all(&pty_root).unwrap();
    let record = |runtime_id: &str, metadata: Value| {
        std::fs::write(
            pty_root.join(format!("{runtime_id}.json")),
            metadata.to_string(),
        )
        .unwrap();
    };
    // This test process stands in for a live session's PTY daemon.
    let running = |runtime_id: &str| {
        std::fs::write(
            pty_root.join(format!("{runtime_id}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
    };
    record(
        RUNTIME_ID,
        json!({ "createdAt": CREATED_AT, "tags": { "st3.subject": SUBJECT } }),
    );
    running(RUNTIME_ID);
    record(
        "example-other",
        json!({ "createdAt": CREATED_AT, "tags": { "st3.subject": "agent/example/other" } }),
    );
    running("example-other");
    // An earlier session of the subject whose daemon wrote its exit record and left.
    record(
        "example-worker-exited",
        json!({
            "createdAt": "2026-09-29T07:00:00.000Z",
            "exitedAt": "2026-09-29T07:59:00.000Z",
            "exitCode": 0,
            "tags": { "st3.subject": SUBJECT },
        }),
    );

    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let output = std::process::Command::new(binary)
        .env_remove("ST_AGENT")
        .env_remove("ST_MISSION_RUN")
        .env_remove("PTY_SESSION")
        .env_remove("ST3_ENDPOINT")
        .env_remove("ST3_DAEMON_WAIT")
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_RUNTIME_DIR", root.path().join("run"))
        .env("PATH", recording_pty(root.path()))
        .args(["--daemon-wait", "0", "terminals", "attach", SUBJECT])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(5), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "PTY_ROOT={} pty attach --no-restart {RUNTIME_ID}",
            pty_root.display()
        )),
        "the session to attach to directly was not named: {stderr}"
    );
    assert!(
        !stderr.contains("example-other") && !stderr.contains("example-worker-exited"),
        "only the subject's running sessions are named: {stderr}"
    );
    assert!(
        !root.path().join("pty-runs").exists(),
        "without its daemon st must not attach or start anything"
    );
}
