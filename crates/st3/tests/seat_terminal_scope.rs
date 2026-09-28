#![cfg(target_os = "linux")]
//! A seat's harness reaches the local daemon through the same Unix socket an operator uses. The
//! daemon binds each connecting process to the seat named by the `ST_AGENT` it or an ancestor
//! was launched with. These tests hold that binding to the seat's own scope.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use serde_json::Value;
use st3::api::AppState;
use st3::model::ClaimInput;
use st3::store::Store;
use tokio::sync::{Notify, watch};

const PEER_RUNTIME: &str = "peer-terminal";
const PEER_INCARNATION: &str = "4242:2026-09-27T00:00:00Z";

/// A stand-in terminal runtime: it reports one running terminal for the peer seat and records
/// every input it is asked to type.
fn fake_terminal_runtime(root: &Path, input_log: &Path) -> PathBuf {
    let path = root.join("fake-pty");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  list) printf '%s' '[{{\"name\":\"{PEER_RUNTIME}\",\"status\":\"running\",\"pid\":4242,\"createdAt\":\"2026-09-27T00:00:00Z\"}}]' ;;\n  send) shift; printf '%s\\n' \"$*\" >> '{}' ;;\n  *) exit 2 ;;\nesac\n",
            input_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn state_with_a_running_peer_seat(root: &Path, pty_binary: PathBuf) -> AppState {
    let store = Store::open_memory("seat-scope").unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "agent/peer".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/peer".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String(PEER_RUNTIME.into())),
                (
                    "incarnation_id".into(),
                    Value::String(PEER_INCARNATION.into()),
                ),
                ("status".into(), Value::String("running".into())),
                ("reachability".into(), Value::String("local".into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    AppState {
        store: Arc::new(store),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "seat-scope".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary,
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

/// Run the st3 CLI as a process launched inside the seat `agent`, the way a harness tool call
/// runs it.
async fn run_cli_as_seat(root: &Path, socket: &Path, agent: &str, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let socket = socket.to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let agent = agent.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("ST_AGENT", agent)
            .arg("--endpoint")
            .arg(socket)
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

/// POST raw JSON from a process launched inside the seat `agent`, bypassing the CLI's own actor
/// checks. Returns `None` when no raw HTTP client is available on this host.
async fn post_as_seat(socket: &Path, agent: &str, path: &str, body: &str) -> Option<String> {
    let socket = socket.to_path_buf();
    let agent = agent.to_owned();
    let url = format!("http://localhost{path}");
    let body = body.to_owned();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("curl")
            .env("ST_AGENT", agent)
            .arg("--silent")
            .arg("--unix-socket")
            .arg(socket)
            .args(["-H", "content-type: application/json", "-X", "POST"])
            .arg("--data")
            .arg(body)
            .arg(url)
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "fails on main: a seat's process can type into another seat's terminal"]
async fn a_seat_process_cannot_type_into_another_seats_terminal() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let input_log = root.path().join("typed-input.log");
    let pty = fake_terminal_runtime(root.path(), &input_log);
    let state = state_with_a_running_peer_seat(root.path(), pty);
    let server_socket = socket.clone();
    // The production daemon listener: each connection is bound to its caller's seat identity.
    let server = tokio::spawn(async move {
        st3::api::serve_unix_bound(&server_socket, st3::api::router(state)).await
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());

    // Control: the listener does bind this caller to its seat, so a graph write in another
    // seat's name is refused by the daemon itself.
    if let Some(refusal) = post_as_seat(
        &socket,
        "agent/own",
        "/v1/claims",
        r#"{"subject":"agent/peer","kind":"harness.observed","actor":"agent/peer","fields":{}}"#,
    )
    .await
    {
        assert!(
            refusal.contains("foreign-agent-actor"),
            "the daemon did not bind the caller to its seat: {refusal}"
        );
    }

    let output = run_cli_as_seat(
        root.path(),
        &socket,
        "agent/own",
        &["terminals", "send", "agent/peer", "yes"],
    )
    .await;
    let typed = std::fs::read_to_string(&input_log).unwrap_or_default();
    server.abort();

    assert!(
        !output.status.success() && typed.is_empty(),
        "a process of seat agent/own typed into seat agent/peer's terminal: exit {:?}, \
         terminal received {typed:?}, stdout {}, stderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
}
