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

/// Delay only graph control reads; the mailbox and all other daemon routes stay responsive.
/// The provider speaks the real app-server protocol, but never calls a model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delayed_delivery_control_holds_visible_native_input_and_recovers_once() {
    if st3::test_support::supervise_test() {
        return;
    }
    use std::sync::atomic::AtomicBool;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = |name: &str| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    };
    let (Some(pty), Some(python)) = (on_path("pty"), on_path("python3")) else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must provide pty and python3"
        );
        eprintln!("skipped: the delayed delivery proof needs pty and python3");
        return;
    };
    let root_path = tempfile::tempdir().unwrap().keep();
    let root = root_path.as_path();
    eprintln!("isolated delivery-control evidence: {}", root.display());
    let socket = root.join("api.sock");
    let state_socket = root.join("state.sock");
    let pty_root = root.join("pty");
    let runtime = PtyRuntime::new(pty_root.clone()).with_binary(pty.to_string_lossy());
    let _cleanup = Cleanup(runtime.clone());
    // Real driver publications run concurrently with views and mailbox reads. Use the
    // daemon's WAL storage; shared-memory fixtures return SQLITE_LOCKED on that mix.
    let store = Arc::new(Store::open(&root.join("claims.sqlite3"), "bootstrap").unwrap());
    let source = format!(
        "version 2\nagent \"eval.codex-bootstrap\" {{ host \"bootstrap\"; workspace {:?}; harness \"codex\" {{}} }}",
        root,
    );
    store
        .apply_internal(
            &st3::graph::parse_intent(&source, "bootstrap").unwrap(),
            "delayed-control",
        )
        .unwrap();
    runtime_claim(&store, "starting", None);
    let delayed = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "bootstrap".into(),
        state_dir: root.into(),
        pty_root: pty_root.clone(),
        pty_binary: pty.clone(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let delay = delayed.clone();
    let read_count = reads.clone();
    let app = st3::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let delay = delay.clone();
            let read_count = read_count.clone();
            async move {
                if request.uri().path() == "/v1/delivery/hold" && request.method() == "GET" {
                    read_count.fetch_add(1, Ordering::SeqCst);
                    if delay.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(350)).await;
                    }
                }
                next.run(request).await
            }
        },
    ));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix_bound(&server_socket, &state_socket, app)
            .await
            .unwrap();
    });
    until(|| socket.exists(), "the isolated API did not start").await;
    let provider = root.join("provider");
    let stub = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/st3-boot-canaries/stub-codex.py");
    std::fs::write(
        &provider,
        format!(
            "#!/bin/sh\nexec env PYTHONDONTWRITEBYTECODE=1 '{}' '{}' \"$@\"\n",
            python.display(),
            stub.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Other cargo jobs can replace the shared target binary. A private copy keeps
    // this control-read proof from accidentally exercising driver re-execution.
    let binary_dir = tempfile::Builder::new()
        .prefix("delivery-control-binary-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let binary = binary_dir.path().join("st3-fixture");
    std::fs::copy(env!("CARGO_BIN_EXE_st3-fixture"), &binary).unwrap();
    let environment = BTreeMap::from([
        ("HOME", root.to_string_lossy().into_owned()),
        ("PATH", path.to_string_lossy().into_owned()),
        ("ST_AGENT", SUBJECT.to_owned()),
        ("ST3_SUBJECT", SUBJECT.to_owned()),
        ("ST3_BIN", binary.to_string_lossy().into_owned()),
        ("ST3_ENDPOINT", socket.to_string_lossy().into_owned()),
        (
            "ST3_DRIVER_STATE_DIR",
            root.join("drivers").to_string_lossy().into_owned(),
        ),
        ("ST3_MAILBOX_TRANSPORT", "push".to_owned()),
    ]);
    let launch_root = root.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(pty);
        command
            .env_clear()
            .env("PTY_ROOT", pty_root)
            .args(["run", "-d", "--force", "--id", RUNTIME, "--cwd"])
            .arg(launch_root)
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
            .arg("--")
            .arg(binary)
            .args(["driver", "codex", "--subject", SUBJECT, "--"])
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
        || {
            store
                .latest_claim(SUBJECT, Some("harness.observed"))
                .unwrap()
                .is_some()
        },
        "the driver did not publish startup",
    )
    .await;
    let observation = runtime
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|p| p.name == RUNTIME)
        .unwrap();
    let incarnation = format!(
        "{}:{}",
        observation.pid.unwrap(),
        observation.created_at.unwrap()
    );
    runtime_claim(&store, "running", Some(&incarnation));
    let client = st3::client::Client::unix(&socket);
    // The in-process API and fixture driver have different executable inodes. Establish
    // the healthy assessment first, so recovery must restore it exactly, including that
    // independent binary-version assessment.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let healthy: Value = loop {
        let agent: Value = client
            .get(&format!("/v1/client/agents/{SUBJECT}"))
            .await
            .unwrap();
        if agent["delivery"]["state"] == "outdated" {
            break agent;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "native control did not become ready: {agent}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    delayed.store(true, Ordering::SeqCst);
    // Idle drivers do not poll control. Unread native mail must make the failed read visible.
    let receipt: st3::model::MessageSendReceipt = client
        .post(
            "/v1/messages",
            &st3::model::MessageSendRequest {
                idempotency_key: "delayed-handoff".into(),
                from: "person/eval".into(),
                to: SUBJECT.into(),
                content: "An invented delayed-control signal.".into(),
                title: None,
                in_reply_to: None,
                tags: vec![],
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let agent: Value = client
            .get(&format!("/v1/client/agents/{SUBJECT}"))
            .await
            .unwrap();
        if agent["delivery"]["state"] == "stale"
            && agent["delivery"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("delivery-control-unavailable:"))
        {
            assert_eq!(agent["state"], "waiting");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "held seat lost its visible reason: {agent}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reference = receipt.message.subject;
    let receipts = root.join("receipts-agent-eval-codex-bootstrap.jsonl");
    let offers = || -> usize {
        std::fs::read_to_string(&receipts)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| {
                row["event"] == "turn"
                    && row["text"]
                        .as_str()
                        .is_some_and(|text| text.contains(&reference))
            })
            .count()
    };
    // Observe several further reads: healthy mailbox traffic cannot clear the block.
    let before = reads.load(Ordering::SeqCst);
    until(
        || reads.load(Ordering::SeqCst) >= before + 4,
        "control reads stopped retrying",
    )
    .await;
    assert_eq!(offers(), 0, "a delayed response authorized native input");
    let delivery: Value = client
        .get(&format!("/v1/messages/delivery/{reference}"))
        .await
        .unwrap();
    assert_eq!(delivery["delivery"]["state"], "waiting");
    assert!(
        delivery["delivery"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("delivery-control-unavailable:")
    );
    delayed.store(false, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = store.message(&reference).unwrap().unwrap().status;
        if status == "read" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recovered control never delivered input: status={status}, offers={}, receipts={}, evidence={}",
            offers(),
            std::fs::read_to_string(&receipts).unwrap_or_default(),
            root.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    until(
        || offers() == 1,
        "the provider did not record the recovered offer",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let agent: Value = client
            .get(&format!("/v1/client/agents/{SUBJECT}"))
            .await
            .unwrap();
        if agent["delivery"]["state"] == healthy["delivery"]["state"]
            && agent["delivery"]["reason"] == healthy["delivery"]["reason"]
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "recovered idle control did not restore readiness: {agent}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Allow consumption to reach the mailbox, then observe further idle driver cycles.
    // No control poll is needed once the native receipt marks the only message read.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = reads.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "idle control resumed polling"
    );
    assert_eq!(offers(), 1, "idle mailbox cycles replayed native input");
    for kind in ["message.staged", "message.delivered", "message.read"] {
        assert_eq!(
            store.claims_for(&reference, Some(kind)).unwrap().len(),
            1,
            "duplicate {kind}"
        );
    }
    server.abort();
    drop(_cleanup);
    std::fs::remove_dir_all(root).unwrap();
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
    if st3::test_support::supervise_test() {
        return;
    }
    bootstrap_waits_for_reconciliation("starting", None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replacement_driver_waits_while_the_previous_runtime_is_vanished() {
    if st3::test_support::supervise_test() {
        return;
    }
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
