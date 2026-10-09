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
const TERMINAL: &str = "pty/person/eval/019a0000-0000-7000-8000-000000000001";

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
    delivery_recovery_control(false, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mailbox_reconnect_preserves_provider_and_consumes_queued_mail_once() {
    if st3::test_support::supervise_test() {
        return;
    }
    delivery_recovery_control(true, false, None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_cold_codex_promotes_custody_delivers_once_and_returns_to_its_shell() {
    if st3::test_support::supervise_test() {
        return;
    }
    delivery_recovery_control(true, true, Some(CompletionControl { order: "before", failure: false, rejection: None })).await;
}

#[derive(Clone, Copy)]
struct CompletionControl {
    order: &'static str,
    failure: bool,
    rejection: Option<&'static str>,
}

macro_rules! codex_completion_control {
    ($name:ident, $order:literal, $failure:literal, $rejection:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            if st3::test_support::supervise_test() { return; }
            delivery_recovery_control(true, true, Some(CompletionControl {
                order: $order, failure: $failure, rejection: $rejection,
            })).await;
        }
    };
}

codex_completion_control!(bound_codex_success_after_join_keeps_its_shell, "after", false, None);
codex_completion_control!(bound_codex_failure_before_join_keeps_failure, "before", true, None);
codex_completion_control!(bound_codex_failure_after_join_keeps_failure, "after", true, None);
codex_completion_control!(bound_codex_foreign_token_after_terminal_stays_fenced, "before", false, Some("token"));
codex_completion_control!(bound_codex_foreign_runtime_after_terminal_stays_fenced, "before", false, Some("runtime"));
codex_completion_control!(bound_codex_foreign_invocation_after_terminal_stays_fenced, "before", false, Some("invocation"));

fn lease_evidence(root: &Path) -> Value {
    use sha2::{Digest as _, Sha256};
    let connection = rusqlite::Connection::open(root.join("claims.sqlite3")).unwrap();
    connection.query_row(
        "SELECT provider,session,sequence,pid,process_token,token,epoch FROM local_mailbox_leases WHERE subject=?1 AND component='delivery'",
        [SUBJECT], |row| {
            let token: String = row.get(5)?;
            Ok(json!({"provider":row.get::<_,String>(0)?,"session":row.get::<_,String>(1)?,
                "sequence":row.get::<_,u64>(2)?,"pid":row.get::<_,u32>(3)?,"process_token":row.get::<_,String>(4)?,
                "binding_hash":hex::encode(Sha256::digest(token.as_bytes())),"epoch":row.get::<_,u64>(6)?}))
        },
    ).unwrap()
}

async fn delivery_recovery_control(
    mailbox_loss: bool,
    bound_shell: bool,
    completion: Option<CompletionControl>,
) {
    use std::sync::atomic::AtomicBool;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = |name: &str| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
    };
    let (Some(pty), Some(python), Some(bash)) =
        (on_path("pty"), on_path("python3"), on_path("bash"))
    else {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must provide pty, python3 and bash"
        );
        eprintln!("skipped: the delayed delivery proof needs pty, python3 and bash");
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
    let mut source = format!(
        "version 2\nagent \"eval.codex-bootstrap\" {{ host \"bootstrap\"; workspace {:?}; harness \"codex\" {{}} }}",
        root,
    );
    if bound_shell {
        source.push_str(&format!(
            "\nterminal {:?} {{ command \"shell\"; restart \"never\"; }}\n",
            TERMINAL.strip_prefix("pty/").unwrap(),
        ));
    }
    if mailbox_loss {
        source.push_str(&format!("\nmission \"mailbox-recovery\" state=\"ready\" {{ goal \"Keep queued work while a real native mailbox is deaf.\"; step \"queued\" {{ assigned-to \"{SUBJECT}\" }} }}\n"));
    }
    let intent = st3::graph::parse_intent(&source, "bootstrap").unwrap();
    let plan = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &plan.subject_tokens,
            "delayed-control",
            Some("person/eval"),
        )
        .unwrap();
    let queued = mailbox_loss.then(|| {
        let run = store
            .create_mission_run(&st3::model::MissionRunRequest {
                mission: "mailbox-recovery".into(),
                revision: None,
                workspace: root.to_string_lossy().into_owned(),
                requester: Some("person/eval".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "mailbox-recovery-run".into(),
            })
            .unwrap();
        let subject = run.steps[0].subject.clone();
        store.set_step_state(&subject, "ready", None).unwrap();
        subject
    });
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
    let bootstrap_custody = Arc::new(std::sync::Mutex::new(None));
    let capture = bootstrap_custody.clone();
    let capture_root = root_path.clone();
    let app = st3::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let delay = delay.clone();
            let read_count = read_count.clone();
            let capture = capture.clone();
            let capture_root = capture_root.clone();
            async move {
                let bind = request.uri().path() == "/v1/mailbox/bind";
                if request.uri().path() == "/v1/delivery/hold" && request.method() == "GET" {
                    read_count.fetch_add(1, Ordering::SeqCst);
                    if delay.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(350)).await;
                    }
                }
                let response = next.run(request).await;
                if bind {
                    let (parts, body) = response.into_parts();
                    let bytes = to_bytes(body, 1024 * 1024).await.unwrap();
                    if parts.status.is_success() {
                        *capture.lock().unwrap() = Some(lease_evidence(&capture_root));
                    } else {
                        eprintln!(
                            "isolated Codex bind refusal: {} {}",
                            parts.status,
                            String::from_utf8_lossy(&bytes)
                        );
                    }
                    return axum::response::Response::from_parts(parts, Body::from(bytes));
                }
                response
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
    let stub = PathBuf::from(test_env!("CARGO_MANIFEST_DIR"))
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
        .tempdir_in(test_env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let binary = binary_dir.path().join("st3-fixture");
    std::fs::copy(test_env!("CARGO_BIN_EXE_st3-fixture"), &binary).unwrap();
    let mut environment = BTreeMap::from([
        ("HOME", root.to_string_lossy().into_owned()),
        ("PATH", path.to_string_lossy().into_owned()),
        ("ST_AGENT", SUBJECT.to_owned()),
        (
            "ST3_SUBJECT",
            if bound_shell { TERMINAL } else { SUBJECT }.to_owned(),
        ),
        ("ST3_BIN", binary.to_string_lossy().into_owned()),
        ("ST3_ENDPOINT", socket.to_string_lossy().into_owned()),
        (
            "ST3_DRIVER_STATE_DIR",
            root.join("drivers").to_string_lossy().into_owned(),
        ),
        ("ST3_MAILBOX_TRANSPORT", "push".to_owned()),
    ]);
    let barrier = root.join("completion-control");
    if let Some(control) = completion {
        std::fs::create_dir(&barrier).unwrap();
        std::fs::write(barrier.join("order"), control.order).unwrap();
        environment.insert("ST3_FIXTURE_TERMINAL_COMPLETION", barrier.to_string_lossy().into_owned());
    }
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
                &format!("st3.subject={}", if bound_shell { TERMINAL } else { SUBJECT }),
            ]);
        for (key, value) in environment {
            command.arg("--env").arg(format!("{key}={value}"));
        }
        if bound_shell {
            command.args(["--", bash.to_str().unwrap(), "--noprofile", "--norc", "-c",
                "while [ ! -e \"$HOME/launch-bound-codex\" ]; do sleep 0.02; done; \"$ST3_BIN\" driver codex --subject \"$ST_AGENT\" -- \"$HOME/provider\"; printf '%s' \"$?\" > \"$HOME/driver-returned\"; while :; do sleep 1; done"]);
        } else {
            command.arg("--").arg(binary).args(["driver", "codex", "--subject", SUBJECT, "--"]).arg(provider);
        }
        command
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
    if !bound_shell {
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
    }
    let observation = runtime
        .snapshot()
        .unwrap()
        .into_iter()
        .find(|p| p.name == RUNTIME)
        .unwrap();
    let physical_incarnation = format!(
        "{}:{}",
        observation.pid.unwrap(),
        observation.created_at.unwrap()
    );
    let shell_pid = if bound_shell {
        let stats: Value = serde_json::from_str(
            &pty_core::registry::with_root(&root.join("pty"), || {
                pty_client::query_status_json(RUNTIME, pty_client::STATS_TIMEOUT)
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            stats["daemon"]["pid"].as_u64(),
            observation.pid.map(u64::from)
        );
        assert_eq!(stats["process"]["alive"], true);
        stats["process"]["pid"].as_u64().unwrap() as u32
    } else {
        observation.pid.unwrap()
    };
    let shell_generation = st_runtime::process_start_token(shell_pid).unwrap();
    let incarnation = if bound_shell {
        let invocation = "019a0000-0000-7000-8000-000000000004";
        let source = format!(
            "version 2\nagent \"eval.codex-bootstrap\" {{ harness \"codex\" {{}}; bind-terminal {TERMINAL:?} incarnation={physical_incarnation:?} id={invocation:?}; }}"
        );
        let intent = st3::graph::parse_intent(&source, "bootstrap").unwrap();
        let plan = store
            .mission(
                &intent,
                st3::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "bind-cold-codex",
                Some("person/eval"),
            )
            .unwrap();
        format!("{physical_incarnation}:bound:{invocation}")
    } else {
        physical_incarnation.clone()
    };
    runtime_claim(&store, "running", Some(&incarnation));
    if bound_shell {
        std::fs::write(root.join("launch-bound-codex"), b"launch").unwrap();
    }
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
        if agent["delivery"]["state"] == "outdated"
            && lease_evidence(root)["sequence"].as_u64().unwrap() > 0
            && matches!(
                agent["harness_state"].as_str(),
                Some("ready" | "idle" | "working")
            )
        {
            break agent;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "native control did not become ready: {agent}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let lease = lease_evidence(root);
    assert!(lease["sequence"].as_u64().unwrap() > 0);
    assert_eq!(lease["provider"], "codex");
    let bootstrap = bootstrap_custody.lock().unwrap().clone().unwrap();
    assert_eq!(bootstrap["sequence"], 0);
    for field in ["pid", "process_token", "binding_hash", "epoch"] {
        assert_eq!(lease[field], bootstrap[field], "promotion changed {field}");
    }
    if mailbox_loss {
        st3::test_support::hold_fixture_mailbox(&store, SUBJECT, true);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let agent: Value = client
                .get(&format!("/v1/client/agents/{SUBJECT}"))
                .await
                .unwrap();
            if agent["fault"].is_string() {
                assert_eq!(agent["driver"], "codex");
                assert_eq!(agent["state"], "waiting");
                assert_eq!(agent["harness_state"], "indeterminate");
                assert_eq!(agent["next_work_id"].as_str(), queued.as_deref());
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "mailbox loss remained invisible: {agent}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let failures = store
            .claims_for(SUBJECT, Some("operational.failure"))
            .unwrap();
        let fault = failures
            .iter()
            .find(|claim| claim.body["fields"]["condition"] == "mailbox-channel-lost")
            .unwrap();
        assert_eq!(fault.body["fields"]["reviewer"], "person/eval");
        assert_eq!(fault.body["fields"]["incarnation"], incarnation);
    } else {
        delayed.store(true, Ordering::SeqCst);
    }
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
    if !mailbox_loss {
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
    if !mailbox_loss {
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
    } else {
        let before = store
            .claims_for(SUBJECT, Some("operational.failure"))
            .unwrap()
            .len();
        store
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: "harness.observed".into(),
                actor: Some(SUBJECT.into()),
                fields: serde_json::from_value(
                    json!({"state":"ready","driver":"codex","incarnation_id":incarnation}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some("superficial-mailbox-ready".into()),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(offers(), 0, "a deaf mailbox handed input to the provider");
        assert_eq!(
            store
                .claims_for(SUBJECT, Some("operational.failure"))
                .unwrap()
                .len(),
            before
        );
        let agent: Value = client
            .get(&format!("/v1/client/agents/{SUBJECT}"))
            .await
            .unwrap();
        assert!(
            agent["fault"].is_string(),
            "superficial readiness erased the loss"
        );
        st3::test_support::hold_fixture_mailbox(&store, SUBJECT, false);
    }
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
        if agent["fault"].is_null()
            && agent["delivery"]["state"] == healthy["delivery"]["state"]
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
    if mailbox_loss {
        assert_eq!(
            lease_evidence(root),
            lease,
            "mailbox reconnect replaced the provider or capability"
        );
        let observation = runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|p| p.name == RUNTIME)
            .unwrap();
        assert_eq!(
            format!(
                "{}:{}",
                observation.pid.unwrap(),
                observation.created_at.unwrap()
            ),
            physical_incarnation
        );
        let recovered = store
            .claims_for(SUBJECT, Some("operational.recovered"))
            .unwrap();
        assert_eq!(recovered.len(), 1);
        let agent: Value = client
            .get(&format!("/v1/client/agents/{SUBJECT}"))
            .await
            .unwrap();
        assert_eq!(agent["driver"], "codex");
        assert_eq!(agent["next_work_id"].as_str(), queued.as_deref());
        std::fs::write(
            root.join("mailbox-recovery-evidence.json"),
            serde_json::to_vec_pretty(&json!({
                "incarnation":incarnation,"lease":lease,"message":reference,"offers":offers(),
                "recovered":recovered[0],"agent":agent,"queued_work":queued,
            }))
            .unwrap(),
        )
        .unwrap();
        eprintln!(
            "isolated authenticated mailbox recovery: {}",
            root.display()
        );
    }
    if bound_shell {
        assert_ne!(
            lease["pid"].as_u64().unwrap(),
            u64::from(shell_pid),
            "bootstrap must admit the wrapper child, not the shell"
        );
        let ready: Value = std::fs::read_to_string(&receipts)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|row| row["event"] == "tui-ready")
            .expect("original provider reached native readiness");
        let pid = ready["pid"].as_u64().unwrap() as u32;
        // Terminate only this model-free fixture's TUI; the wrapper's owned native exit
        // must return to the same surviving physical shell.
        assert!(
            std::fs::read_to_string(format!("/proc/{pid}/environ"))
                .unwrap()
                .contains(root.to_str().unwrap())
        );
        let signal = if completion.is_some_and(|control| control.failure) { libc::SIGKILL } else { libc::SIGTERM };
        assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0);
        let mut terminal_result = Value::Null;
        if let Some(control) = completion {
            until(|| barrier.join("provider-return.json").exists(), "Codex provider result was not retained").await;
            terminal_result = serde_json::from_slice(&std::fs::read(barrier.join("provider-return.json")).unwrap()).unwrap();
            assert_eq!(terminal_result["ok"], !control.failure, "{terminal_result}");
            until(|| barrier.join("observation-drained").exists(), "live Codex drain did not defer its owned terminal event").await;
            assert_eq!(std::fs::read_to_string(barrier.join("observation-drained")).unwrap(), "deferred");
            if control.order == "after" { std::fs::write(barrier.join("release-provider"), b"go").unwrap(); }
            until(|| barrier.join("join-phase").exists(), "Codex JoinHandle phase was not observed").await;
            assert_eq!(std::fs::read_to_string(barrier.join("join-phase")).unwrap(),
                if control.order == "after" { "finished" } else { "pending" });
            let latest = store.latest_claim(SUBJECT, Some("harness.observed")).unwrap().unwrap();
            assert_ne!(latest.body["fields"]["state"], "ended", "terminal graph publication must wait for the task disposition");
            let retained = lease_evidence(root);
            assert_eq!(retained, lease, "completion does not reissue custody");
            match control.rejection {
                Some("token") => {
                    let request = st3::mailbox::Fence::new(SUBJECT, &incarnation, "delivery");
                    st3::test_support::bind_fixture_mailbox(&store, &request).unwrap();
                }
                Some("runtime") => {
                    store.append_claim(&ClaimInput {
                        subject: SUBJECT.into(), kind: "runtime.observed".into(), actor: Some("daemon/runtime".into()),
                        fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!("replacement-runtime"))]),
                        evidence: vec![], expected_subject: None, idempotency_key: None,
                    }).unwrap();
                }
                Some("invocation") => {
                    let source = format!("version 2\nagent \"eval.codex-bootstrap\" {{ harness \"codex\" {{}}; bind-terminal {TERMINAL:?} incarnation={physical_incarnation:?} id=\"019a0000-0000-7000-8000-000000000005\"; }}");
                    let intent = st3::graph::parse_intent(&source, "bootstrap").unwrap();
                    let plan = store.mission(&intent, st3::model::IntentInput { kdl: source, source_name: None }).unwrap();
                    store.apply_as(&intent, &plan.subject_tokens, "replace-codex-invocation", Some("person/eval")).unwrap();
                }
                None => {},
                Some(other) => panic!("unknown completion rejection {other}"),
            }
            std::fs::write(barrier.join("poll-driver"), b"go").unwrap();
            if control.order == "before" && control.rejection.is_none() {
                std::fs::write(barrier.join("release-provider"), b"go").unwrap();
            }
            if control.rejection.is_some() {
                // Prove the foreign fence is fatal while the provider JoinHandle is
                // pending, then release the blocking fixture worker for shutdown.
                until(|| barrier.join("fence-received").exists(), "foreign Codex fence was not accepted").await;
                assert_eq!(std::fs::read_to_string(barrier.join("fence-received")).unwrap(), "pending");
                std::fs::write(barrier.join("release-provider"), b"go").unwrap();
            }
        }
        until(
            || root.join("driver-returned").exists(),
            "the bound wrapper did not return to its shell",
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(root.join("driver-returned")).unwrap(),
            if completion.is_some_and(|control| control.failure || control.rejection.is_some()) { "2" } else { "0" }
        );
        if let Some(control) = completion {
            if control.failure {
                assert!(store.claims_for(SUBJECT, Some("harness.diagnostic")).unwrap().iter()
                    .any(|claim| claim.body["fields"]["code"] == "codex-driver-failed"));
            }
            assert_eq!(offers(), 1, "terminal disposition must not replay native mail");
            std::fs::write(root.join("codex-completion-evidence.json"), serde_json::to_vec_pretty(&json!({
                "order":control.order,"failed_provider":control.failure,"foreign_fence":control.rejection,
                "provider_result":terminal_result,"wrapper_exit":std::fs::read_to_string(root.join("driver-returned")).unwrap(),
                "join_phase":std::fs::read_to_string(barrier.join("join-phase")).unwrap(),
                "original_lease":lease,"offers":offers(),"same_shell_pid":shell_pid,"same_shell_birth":shell_generation,
            })).unwrap()).unwrap();
        }
        assert_eq!(
            st_runtime::process_start_token(shell_pid).unwrap(),
            shell_generation
        );
        assert_eq!(
            runtime
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|p| p.name == RUNTIME)
                .unwrap()
                .status,
            "running"
        );
    }
    server.abort();
    drop(_cleanup);
    if !mailbox_loss {
        std::fs::remove_dir_all(root).unwrap();
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
    let source = format!(
        "version 2\nagent \"eval.codex-bootstrap\" {{ host \"bootstrap\"; workspace {:?}; harness \"codex\" {{}} }}",
        root.path()
    );
    let intent = st3::graph::parse_intent(&source, "bootstrap").unwrap();
    store
        .apply_internal(&intent, "bootstrap-declared-seat")
        .unwrap();
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
                if !parts.status.is_success() {
                    eprintln!("isolated Codex bootstrap refusal: {} {value}", parts.status);
                }
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
        .unwrap_or_else(|| PathBuf::from(test_env!("CARGO_BIN_EXE_st3-fixture")));
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
