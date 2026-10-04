//! A Codex wrapper must survive the interval between local PTY publication and the
//! reconciler's runtime.running claim. No model or credentials are used in this proof.
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use serde_json::{Value, json};
use st_runtime::PtyRuntime;
use st3::api::AppState;
use st3::model::ClaimInput;
use st3::store::Store;
use tokio::sync::{Notify, watch};

const SUBJECT: &str = "agent/eval.codex-bootstrap";
const RUNTIME: &str = "codex-bootstrap";

fn runtime_claim(store: &Store, status: &str, incarnation: Option<&str>) {
    let mut fields = BTreeMap::from([
        ("status".into(), json!(status)),
        ("runtime_id".into(), json!(RUNTIME)),
        ("host".into(), json!("bootstrap")),
    ]);
    if let Some(incarnation) = incarnation {
        fields.insert("incarnation_id".into(), json!(incarnation));
    }
    store
        .append_claim(&ClaimInput {
            subject: SUBJECT.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("bootstrap-runtime-{status}")),
        })
        .unwrap();
}

struct Cleanup(PtyRuntime);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.stop(RUNTIME);
        let _ = self.0.remove(RUNTIME);
    }
}

async fn until(mut predicate: impl FnMut() -> bool, description: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(tokio::time::Instant::now() < deadline, "{description}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_codex_driver_waits_for_reconciliation_before_binding_its_mailbox() {
    bootstrap_waits_for_reconciliation("starting", None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replacement_driver_waits_while_the_previous_runtime_is_vanished() {
    bootstrap_waits_for_reconciliation("vanished", Some("previous-incarnation")).await;
}

async fn bootstrap_waits_for_reconciliation(status: &str, previous: Option<&str>) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pty = std::env::split_paths(&path)
        .map(|dir| dir.join("pty"))
        .find(|p| p.is_file());
    let Some(pty) = pty else {
        assert!(std::env::var_os("CI").is_none(), "CI must provide pty");
        eprintln!("skipped: the Codex bootstrap proof needs pty on PATH");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("api.sock");
    let state_socket = root.path().join("state.sock");
    let pty_root = root.path().join("pty");
    let runtime = PtyRuntime::new(pty_root.clone()).with_binary(pty.to_string_lossy());
    let _cleanup = Cleanup(runtime.clone());
    let store = Arc::new(Store::open_memory("bootstrap").unwrap());
    runtime_claim(&store, status, previous);
    let pending = Arc::new(AtomicUsize::new(0));
    let bound = Arc::new(AtomicUsize::new(0));
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "bootstrap".into(),
        state_dir: root.path().into(),
        pty_root: pty_root.clone(),
        pty_binary: pty.clone(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let pending_count = pending.clone();
    let bound_count = bound.clone();
    let app = st3::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let pending = pending_count.clone();
            let bound = bound_count.clone();
            async move {
                let binding = request.uri().path() == "/v1/mailbox/bind";
                let response = next.run(request).await;
                if !binding {
                    return response;
                }
                let (parts, body) = response.into_parts();
                let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                if value["code"] == "mailbox-session-starting" {
                    pending.fetch_add(1, Ordering::SeqCst);
                }
                if parts.status.is_success() {
                    assert_eq!(
                        value["value"]["epoch"], 1,
                        "pending binds allocate no owner"
                    );
                    bound.fetch_add(1, Ordering::SeqCst);
                }
                axum::response::Response::from_parts(parts, Body::from(bytes))
            }
        },
    ));
    let server_socket = socket.clone();
    let server_state_socket = state_socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix_bound(&server_socket, &server_state_socket, app)
            .await
            .unwrap();
    });
    until(|| socket.exists(), "the isolated API did not start").await;
    let provider = root.path().join("provider");
    let marker = root.path().join("provider-invoked");
    std::fs::write(
        &provider,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 42\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let binary = std::env::var_os("ST3_BOOTSTRAP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_st3-fixture")));
    let binary_path = binary.to_string_lossy().into_owned();
    let environment = BTreeMap::from([
        ("HOME", root.path().to_string_lossy().into_owned()),
        ("PATH", path.to_string_lossy().into_owned()),
        ("ST_AGENT", SUBJECT.to_owned()),
        ("ST3_SUBJECT", SUBJECT.to_owned()),
        ("ST3_BIN", binary_path.clone()),
        ("ST3_ENDPOINT", socket.to_string_lossy().into_owned()),
        (
            "ST3_DRIVER_STATE_DIR",
            root.path().join("drivers").to_string_lossy().into_owned(),
        ),
        ("ST3_MAILBOX_TRANSPORT", "push".to_owned()),
    ]);
    let result = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(pty);
        command
            .env_clear()
            .env("PTY_ROOT", &pty_root)
            .args(["run", "-d", "--force", "--id", RUNTIME, "--cwd"])
            .arg(Path::new(&environment["HOME"]))
            .args([
                "--tag",
                "keep=true",
                "--tag",
                &format!("st3.subject={SUBJECT}"),
            ]);
        for (key, value) in environment {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        command
            .args([
                "--",
                &binary_path,
                "driver",
                "codex",
                "--subject",
                SUBJECT,
                "--",
            ])
            .arg(provider)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    until(
        || pending.load(Ordering::SeqCst) > 0,
        "the driver exited instead of waiting for runtime.running (missing startup observation)",
    )
    .await;
    assert!(
        !marker.exists(),
        "the provider launched before mailbox ownership was established"
    );
    assert_eq!(bound.load(Ordering::SeqCst), 0);
    let harness = store
        .latest_claim(SUBJECT, Some("harness.observed"))
        .unwrap()
        .unwrap();
    assert_eq!(harness.body["fields"]["state"], "starting");
    assert_eq!(harness.body["fields"]["driver"], "codex");
    let observation = runtime
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|p| p.name == RUNTIME)
        .unwrap();
    assert_eq!(
        observation.status, "running",
        "the native wrapper survived the startup window"
    );
    let incarnation = format!(
        "{}:{}",
        observation.pid.unwrap(),
        observation.created_at.unwrap()
    );
    assert_eq!(harness.body["fields"]["incarnation_id"], incarnation);
    runtime_claim(&store, "running", Some(&incarnation));
    until(
        || marker.exists(),
        "the provider did not launch after its mailbox bound",
    )
    .await;
    assert_eq!(bound.load(Ordering::SeqCst), 1);
    // The proof launches only a stub provider on an isolated PTY; cleanup ends that session.
    server.abort();
}
