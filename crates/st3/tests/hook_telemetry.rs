//! The real hook executable must never build an exporter or wait for a collector.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use st3::model::ClaimInput;
use st3::store::Store;

#[derive(Clone)]
struct Daemon {
    store: Arc<Store>,
    received: Arc<Mutex<Vec<Value>>>,
    delay: Duration,
}

async fn admit(State(daemon): State<Daemon>, Json(claim): Json<ClaimInput>) -> Json<Value> {
    let record = daemon.store.append_claim(&claim).unwrap();
    daemon
        .received
        .lock()
        .unwrap()
        .push(serde_json::to_value(&claim).unwrap());
    tokio::time::sleep(daemon.delay).await;
    Json(serde_json::to_value(record).unwrap())
}

async fn start_daemon(socket: &Path, delay: Duration) -> (Daemon, tokio::task::JoinHandle<()>) {
    let daemon = Daemon {
        store: Arc::new(Store::open_memory("example-node").unwrap()),
        received: Arc::new(Mutex::new(Vec::new())),
        delay,
    };
    let app = Router::new()
        .route("/v1/claims", post(admit))
        .with_state(daemon.clone());
    let path = socket.to_owned();
    let server = tokio::spawn(async move { st3::api::serve_unix(&path, app).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (daemon, server)
}

fn seat(root: &Path) -> std::path::PathBuf {
    let catalog = root.join("catalog");
    let host = st_drivers::run::detect_host();
    let agent = catalog.join("agents").join(&host).join("seat");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty-root \"/tmp/example-pty\" }\n",
    )
    .unwrap();
    std::fs::write(agent.join("agent.kdl"), format!(
        "agent \"example/seat\" {{\n identity \"example/seat\"\n host {host:?}\n workspace \"/tmp\"\n command \"true\"\n}}\n",
    )).unwrap();
    agent
}

fn hook(root: &Path, event: &str, collector: &str) -> tokio::process::Command {
    let mut command = st3::test_support::async_command(env!("CARGO_BIN_EXE_st3-fixture"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("CATALOG", root.join("catalog"))
        .env("ST_CLAUDE_IDENTITY", "example/seat")
        .env("ST_CLAUDE_RUNTIME_ID", "example/seat")
        .env("ST_CLAUDE_SESSION", "incarnation-1")
        .env("ST_CLAUDE_SESSION_SEQ", "1")
        .env("ST3_SUBJECT", "agent/example/seat")
        .env("ST_AGENT", "agent/example/seat")
        .env("ST3_DRIVER_STATE_DIR", root.join("drivers"))
        .env("ST3_ENDPOINT", root.join("daemon.sock"))
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", collector)
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "10000")
        .env("RUST_LOG", "off")
        .args(["driver-hook", "claude-observe", event])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn an_event_hook_hands_count_and_warnings_to_the_daemon_without_contacting_the_collector() {
    let root = tempfile::tempdir().unwrap();
    let agent = seat(root.path());
    // A failed timeline write is fail-open and must still be observable with RUST_LOG=off.
    std::fs::create_dir(st_drivers::harness_timeline::timeline_path(&agent)).unwrap();
    let (daemon, server) = start_daemon(&root.path().join("daemon.sock"), Duration::ZERO).await;
    let collector = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    collector.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", collector.local_addr().unwrap());
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        hook(root.path(), "PreToolUse", &endpoint).output(),
    )
    .await
    .expect("a hook cannot wait for collector flush")
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let claims = daemon.received.lock().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["kind"], "harness.telemetry");
    assert_eq!(
        claims[0]["fields"]["signals"]["hook_invocations"],
        json!([{"hook":"claude-observe", "event":"PreToolUse"}])
    );
    let logs = claims[0]["fields"]["signals"]["logs"].as_array().unwrap();
    assert!(logs.iter().any(|log| log["severity"] == "WARN"
        && log.to_string().contains("harness-timeline write failed")));
    assert_eq!(daemon.store.index().unwrap(), 0);
    assert_eq!(
        daemon.store.local_observations_after(0, 10).unwrap().len(),
        1
    );
    assert!(
        collector.accept().is_err(),
        "the hook built no collector connection"
    );
    server.abort();
}

#[tokio::test]
async fn an_undeclared_application_and_the_status_line_create_no_hook_count_or_exporter() {
    let root = tempfile::tempdir().unwrap();
    seat(root.path());
    let (daemon, server) = start_daemon(&root.path().join("daemon.sock"), Duration::ZERO).await;
    let collector = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    collector.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", collector.local_addr().unwrap());
    let mut command = hook(root.path(), "PreToolUse", &endpoint);
    command.env("ST_CLAUDE_IDENTITY", "example/missing");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap()
            .status
            .success()
    );
    let mut status_line = st3::test_support::async_command(env!("CARGO_BIN_EXE_st3-fixture"));
    status_line
        .env_clear()
        .env("HOME", root.path())
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .args(["driver-hook", "claude-statusline"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), status_line.output())
            .await
            .unwrap()
            .unwrap()
            .status
            .success()
    );
    assert!(daemon.received.lock().unwrap().is_empty());
    assert!(collector.accept().is_err());
    server.abort();
}

#[tokio::test]
async fn a_daemon_that_delays_telemetry_acknowledgement_cannot_fail_or_hold_up_the_hook() {
    let root = tempfile::tempdir().unwrap();
    seat(root.path());
    let (daemon, server) =
        start_daemon(&root.path().join("daemon.sock"), Duration::from_secs(10)).await;
    let mut command = hook(root.path(), "PreToolUse", "http://127.0.0.1:1");
    let output = tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .expect("the 250 ms admission timeout must release the hook")
        .unwrap();
    assert!(output.status.success());
    assert_eq!(daemon.received.lock().unwrap().len(), 1);
    server.abort();
}
