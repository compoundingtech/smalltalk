#![cfg(unix)]
//! Real finite fake harnesses behind the runtime, API and CLI. No fleet seats are touched.
use serde_json::{Value, json};
use st3::{
    api::AppState,
    model::{ClaimInput, MemberSpec},
    reconcile::{Reconciler, RuntimeControl, RuntimeObservation},
    store::Store,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

struct Processes {
    exec: st_runtime::ExecRuntime,
    ids: Mutex<BTreeSet<String>>,
    starts: Mutex<usize>,
    code: i32,
    root: PathBuf,
}
impl Processes {
    fn new(root: &Path, code: i32) -> Self {
        st_runtime::initialize_isolation(&BTreeMap::new());
        Self {
            exec: st_runtime::ExecRuntime::new(root.join("processes"), root.join("logs")),
            ids: Mutex::new(BTreeSet::new()),
            starts: Mutex::new(0),
            code,
            root: root.into(),
        }
    }
    fn observation(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(self.exec.observe(id)?.map(|observed| match observed {
            st_runtime::ExecObservation::Running(g) => RuntimeObservation {
                runtime_id: id.into(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(g.generation_id),
            },
            st_runtime::ExecObservation::Exited(g) => RuntimeObservation {
                runtime_id: id.into(),
                terminal: true,
                status: "exited".into(),
                exit_code: g.exit_code.map(i64::from),
                incarnation_id: Some(g.generation_id),
            },
            st_runtime::ExecObservation::Indeterminate(reason) => {
                panic!("fixture process indeterminate: {reason}")
            }
        }))
    }
}
impl RuntimeControl for Processes {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        self.ids
            .lock()
            .unwrap()
            .iter()
            .map(|id| self.observation(id))
            .collect::<anyhow::Result<Vec<_>>>()
            .map(|items| items.into_iter().flatten().collect())
    }
    fn observe_exec(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        self.observation(id)
    }
    fn start(&self, member: &MemberSpec) -> anyhow::Result<()> {
        *self.starts.lock().unwrap() += 1;
        self.ids.lock().unwrap().insert(member.runtime_id.clone());
        let ending = if self.code == 127 {
            format!(
                "exec {}",
                self.root.join("missing-fixture-executable").display()
            )
        } else {
            format!("exit {}", self.code)
        };
        let script = format!(
            "printf 'fixture-stdout-marker\\n'; printf 'fixture-stderr-marker\\n' >&2; {ending}"
        );
        self.exec.spawn(
            &member.runtime_id,
            &st_runtime::Launch::Argv(vec![
                env!("ST3_FIXTURE_BASH").into(),
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                script,
            ]),
            &self.root,
            &BTreeMap::new(),
        )?;
        // Deliberately reap before launch acceptance, exercising exit-before-observation.
        for _ in 0..100 {
            if self
                .observation(&member.runtime_id)?
                .is_some_and(|o| o.status == "exited")
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        anyhow::bail!("finite fixture harness failed to exit")
    }
    fn stop(&self, id: &str, _: bool, expected: Option<&str>) -> anyhow::Result<()> {
        self.exec.stop_if(id, expected)
    }
    fn kill(&self, id: &str, _: bool, expected: Option<&str>) -> anyhow::Result<()> {
        self.exec.kill_if(id, expected)
    }
    fn remove(&self, id: &str, _: bool) -> anyhow::Result<()> {
        self.exec.remove(id)
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, id: &str) -> anyhow::Result<Option<String>> {
        self.exec.read_log_tail(id)
    }
    fn exit_tail(&self, observation: &RuntimeObservation) -> anyhow::Result<Option<String>> {
        self.exec.read_log_tail(&observation.runtime_id)
    }
}
pub(super) fn state(root: &Path, store: Arc<Store>) -> AppState {
    AppState {
        store,
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "studio".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    }
}
fn declare(store: &Store, source: &str, key: &str) {
    let intent = st3::parse_intent(source, "studio").unwrap();
    let preview = store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(&intent, &preview.subject_tokens, key, Some("person/avery"))
        .unwrap();
}
fn claim(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn ready_receipt(store: &Store, subject: &str, incarnation: &str) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: serde_json::from_value(json!({
                "action":"harness-ready", "incarnation_id":incarnation,
            }))
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("launch-ready:{subject}:{incarnation}")),
        })
        .unwrap();
}

pub(super) fn requested_launch() -> (Store, String, &'static str) {
    let store = Store::open_memory("studio").unwrap();
    let subject = "agent/example/worker";
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart always; }",
        "declare",
    );
    let token = store.selected_desired_token(subject).unwrap().unwrap();
    (store, token, subject)
}

#[test]
fn the_first_launch_failure_is_not_replaced_by_a_successful_retry() {
    let (store, token, subject) = requested_launch();
    claim(
        &store,
        subject,
        "runtime.action.failed",
        json!({
            "action":"start", "desired_token":token, "reason":"invented initial launch failure",
        }),
    );
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({
            "action":"start", "desired_token":token, "incarnation_id":"retry",
        }),
    );
    ready_receipt(&store, subject, "retry");
    let status = st3::agent_launch::read(&store, subject, &token).unwrap();
    assert_eq!(status.stage, "launch-failed");
    assert_eq!(
        status.reason.as_deref(),
        Some("invented initial launch failure")
    );
    assert_eq!(status.incarnation_id, None);
}

#[test]
fn omitted_evidence_in_each_window_refuses_readiness_instead_of_masking_failure() {
    for kind in [
        "runtime.action.succeeded",
        "runtime.action.failed",
        "runtime.observed",
        "harness.diagnostic",
    ] {
        let (store, token, subject) = requested_launch();
        claim(
            &store,
            subject,
            "runtime.action.succeeded",
            json!({
                "action":"start", "desired_token":token, "incarnation_id":"first",
            }),
        );
        ready_receipt(&store, subject, "first");
        // The diagnostic window is consulted on the exit path; the other windows must
        // refuse before a missing old failure could turn into a readiness result.
        if kind == "harness.diagnostic" {
            claim(
                &store,
                subject,
                "runtime.observed",
                json!({
                    "status":"exited", "incarnation_id":"first", "exit_code":23,
                }),
            );
        }
        for n in 0..66 {
            let fields = match kind {
                "runtime.action.succeeded" => {
                    json!({"action":"start", "desired_token":format!("other-{n}"), "incarnation_id":format!("other-{n}")})
                }
                "runtime.action.failed" => {
                    json!({"action":"start", "desired_token":format!("other-{n}"), "reason":format!("other failure {n}")})
                }
                "runtime.observed" => {
                    json!({"status":"running", "incarnation_id":format!("other-{n}")})
                }
                _ => {
                    json!({"code":"launch-exited-before-ready", "incarnation_id":format!("other-{n}"), "reason":"invented fixed diagnostic"})
                }
            };
            claim(&store, subject, kind, fields);
        }
        let error = st3::agent_launch::read(&store, subject, &token).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&format!("requested {kind} evidence exceeds")),
            "{kind}: {error:#}"
        );
    }
}

#[test]
fn a_readiness_milestone_is_incarnation_bound_and_an_exit_takes_precedence() {
    let (store, token, subject) = requested_launch();
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({
            "action":"start", "desired_token":token, "incarnation_id":"first",
        }),
    );
    ready_receipt(&store, subject, "other");
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .stage,
        "launched"
    );
    ready_receipt(&store, subject, "first");
    // Subsequent harness input state does not revoke an observed launch milestone.
    claim(
        &store,
        subject,
        "harness.observed",
        json!({"state":"blocked", "incarnation_id":"first"}),
    );
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .stage,
        "harness-ready"
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"exited", "incarnation_id":"first", "exit_code":0}),
    );
    let status = st3::agent_launch::read(&store, subject, &token).unwrap();
    assert_eq!(status.stage, "exited");
    assert_eq!(status.exit_code, Some(0));
}

#[test]
fn relabeling_keeps_the_first_launch_but_a_later_launch_revision_does_not() {
    let (store, token, subject) = requested_launch();
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({
            "action":"start", "desired_token":token, "incarnation_id":"first",
        }),
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"exited", "incarnation_id":"first", "exit_code":17}),
    );
    store
        .rename_agent(subject, Some("Invented label"), "rename")
        .unwrap();
    let relabeled = store.selected_desired_token(subject).unwrap().unwrap();
    assert_ne!(token, relabeled);
    let status = st3::agent_launch::read(&store, subject, &relabeled).unwrap();
    assert_eq!(status.desired_token, relabeled);
    assert_eq!(status.exit_code, Some(17));
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"false\"; restart always; }",
        "different-launch",
    );
    let replacement = store.selected_desired_token(subject).unwrap().unwrap();
    let pending = st3::agent_launch::read(&store, subject, &replacement).unwrap();
    assert_eq!(pending.stage, "replicated");
    assert_eq!(pending.incarnation_id, None);
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart always; }",
        "return-to-original-command",
    );
    let returned = store.selected_desired_token(subject).unwrap().unwrap();
    let pending = st3::agent_launch::read(&store, subject, &returned).unwrap();
    assert_eq!(pending.stage, "replicated");
    assert_eq!(pending.incarnation_id, None);
    assert_eq!(
        st3::agent_launch::read(&store, subject, &relabeled)
            .unwrap()
            .exit_code,
        Some(17)
    );
}

#[test]
fn excessive_label_lineage_is_refused_without_following_a_later_desired_head() {
    let (store, token, subject) = requested_launch();
    for n in 0..65 {
        store
            .rename_agent(subject, Some(&format!("Label {n}")), &format!("rename-{n}"))
            .unwrap();
    }
    let current = store.selected_desired_token(subject).unwrap().unwrap();
    let error = st3::agent_launch::read(&store, subject, &current).unwrap_err();
    assert!(error.to_string().contains("read budget"), "{error:#}");
    // A later head's excess lineage also refuses a pinned older request, rather than
    // accepting an unverified equivalence or folding an unbounded lineage.
    assert!(st3::agent_launch::read(&store, subject, &token).is_err());
}

#[test]
fn readiness_poll_work_does_not_grow_with_unread_harness_history() {
    use smallclaims::sqlite::work::SqliteWorkScope;
    let (store, token, subject) = requested_launch();
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({
            "action":"start", "desired_token":token, "incarnation_id":"first",
        }),
    );
    ready_receipt(&store, subject, "first");
    let mut samples = Vec::new();
    let mut retained = 0;
    for count in [7, 71, 701] {
        for n in 0..count {
            claim(
                &store,
                subject,
                "harness.observed",
                json!({
                    "state":"working", "incarnation_id":"first", "driver":"pi", "reason":format!("history {count}-{n}"),
                }),
            );
        }
        retained += count;
        assert_eq!(
            store
                .claims_for(subject, Some("harness.observed"))
                .unwrap()
                .len(),
            retained,
            "the growth control must keep real durable history"
        );
        let scope = SqliteWorkScope::start();
        let status = st3::agent_launch::read(&store, subject, &token).unwrap();
        let work = scope.finish();
        assert_eq!(status.stage, "harness-ready");
        assert!(
            work.statements > 0 && work.vm_steps > 0,
            "instrumentation must be active"
        );
        assert_eq!(work.fullscan_steps, 0, "{count}: {work:?}");
        assert_eq!(work.autoindex_rows, 0, "{count}: {work:?}");
        samples.push(work);
    }
    assert_eq!(samples[0].statements, samples[2].statements, "{samples:?}");
    assert!(
        samples[2].vm_steps <= samples[0].vm_steps + 100,
        "{samples:?}"
    );
    println!("launch readiness history 7/78/779: {samples:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conditional_launch_wait_keeps_actor_and_releases_its_snapshot_before_notification() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt as _;
    let root = tempfile::tempdir().unwrap();
    let (store, token, subject) = requested_launch();
    let store = Arc::new(store);
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({
            "action":"start", "desired_token":token, "incarnation_id":"first",
        }),
    );
    let initial = st3::agent_launch::read(&store, subject, &token).unwrap();
    let marker = st3::agent_launch::wait_marker(&initial).unwrap();
    let state = state(root.path(), store.clone());
    let events = state.event_notify.clone();
    let app = st3::api::router(state);
    let path = format!("/v1/hosts/studio/agent-launch?subject={subject}&token={token}");
    let anonymous = app
        .clone()
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(!anonymous.status().is_success());
    let body: Value =
        serde_json::from_slice(&to_bytes(anonymous.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(body["code"], "missing-actor");
    let request = Request::get(format!("{path}&after={marker}&wait_ms=1000"))
        .header("x-st3-person", "agent/example/requester")
        .body(Body::empty())
        .unwrap();
    let waiting = tokio::spawn(app.oneshot(request));
    tokio::time::sleep(Duration::from_millis(20)).await;
    tokio::task::spawn_blocking(move || ready_receipt(&store, subject, "first"))
        .await
        .unwrap();
    events.send_modify(|generation| *generation = generation.saturating_add(1));
    let response = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(response.status().is_success());
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(body["value"]["stage"], "harness-ready");
    assert_eq!(body["value"]["incarnation_id"], "first");
}
async fn early_exit_cli(code: i32, structured: bool, attach: bool) {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "studio").unwrap());
    let runtime = Arc::new(Processes::new(root.path(), code));
    let state = state(root.path(), store.clone());
    let events = state.event_notify.clone();
    let reconciler = Arc::new(Reconciler::new(
        store.clone(),
        runtime.clone(),
        "studio".into(),
        state.notify.clone(),
    ));
    let socket = root.path().join("api.sock");
    let server_socket = socket.clone();
    let router = st3::api::router(state).layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            // The runtime below owns an invented provider; no installed fleet harness is used.
            if request.uri().path() == "/v1/harnesses" {
                return axum::response::IntoResponse::into_response(axum::Json(json!({
                    "api_version":"st3.v1", "value":["pi"]
                })));
            }
            next.run(request).await
        },
    ));
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, router).await });
    // serve_unix owns a borrowed path only until this enclosing task ends.
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let drive = tokio::spawn(async move {
        loop {
            reconciler.reconcile_once().unwrap();
            // Native supervise emits this notification after an admitted pass too.
            events.send_modify(|generation| *generation = generation.saturating_add(1));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let mut command = st3::test_support::async_command(test_env!("CARGO_BIN_EXE_st3-fixture"));
    command
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null());
    command.args(["--endpoint", socket.to_str().unwrap(), "--daemon-wait", "0"]);
    if structured {
        command.arg("--json");
    }
    command.args([
        "agents",
        "new",
        "example/early-exit",
        "--host",
        "studio",
        "--workspace",
        root.path().to_str().unwrap(),
        "--harness",
        "pi",
        "--model",
        "invented-model",
        "--timeout",
        "5s",
        "--as",
        "person/avery",
    ]);
    if attach {
        command.arg("--attach");
    }
    let started = Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    drive.abort();
    server.abort();
    assert_eq!(output.status.code(), Some(code.max(1)), "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "exit was not prompt: {:?}",
        started.elapsed()
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("exit code"), "{stderr}");
    assert!(
        stderr.contains("fixture-stdout-marker") && stderr.contains("fixture-stderr-marker"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("404") && !stderr.contains("not ready after"),
        "{stderr}"
    );
    assert_eq!(*runtime.starts.lock().unwrap(), 1);
    let durable =
        serde_json::to_string(&store.claims_for("agent/example/early-exit", None).unwrap())
            .unwrap();
    for marker in ["fixture-stdout-marker", "fixture-stderr-marker"] {
        assert!(
            !durable.contains(marker),
            "startup output entered the replicated claim log"
        );
    }
    if structured {
        let result: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(result["error"]["exit_code"], code);
        assert!(
            result["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["stage"] == "replicated" && stage["state"] == "skipped")
        );
        assert!(!stderr.contains("declared (completed)"));
    } else {
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains("local launch; replication skipped"));
        assert!(stderr.contains("declared") && stderr.contains("exited"));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_returns_nonzero_exit_and_both_streams_before_readiness() {
    early_exit_cli(23, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_fails_creation_while_preserving_zero_child_exit_in_json() {
    early_exit_cli(0, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_new_reports_the_missing_harness_executable_exit() {
    early_exit_cli(127, true, true).await;
}

#[test]
fn requested_failure_survives_another_incarnation_and_source_replacement() {
    let store = Store::open_memory("studio").unwrap();
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart always; }",
        "declare",
    );
    let subject = "agent/example/worker";
    let token = store.selected_desired_token(subject).unwrap().unwrap();
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({"action":"start","desired_token":token,"incarnation_id":"first"}),
    );
    ready_receipt(&store, subject, "first");
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .stage,
        "harness-ready"
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"exited","incarnation_id":"first","exit_code":17}),
    );
    claim(
        &store,
        subject,
        "runtime.action.succeeded",
        json!({"action":"start","desired_token":token,"incarnation_id":"second"}),
    );
    claim(
        &store,
        subject,
        "runtime.observed",
        json!({"status":"running","incarnation_id":"second"}),
    );
    claim(
        &store,
        subject,
        "harness.observed",
        json!({"state":"ready","incarnation_id":"second"}),
    );
    ready_receipt(&store, subject, "second");
    let status = st3::agent_launch::read(&store, subject, &token).unwrap();
    assert_eq!(status.stage, "exited");
    assert_eq!(status.exit_code, Some(17));
    assert_eq!(status.incarnation_id.as_deref(), Some("first"));
    declare(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"false\"; restart always; }",
        "replace",
    );
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .exit_code,
        Some(17)
    );
}

#[test]
fn local_declaration_does_not_prove_destination_replication_or_readiness() {
    let store = Store::open_memory("studio").unwrap();
    declare(
        &store,
        "version 2\nagent \"example/remote\" { host \"workshop\"; workspace \"/tmp\"; command \"true\"; }",
        "declare",
    );
    let subject = "agent/example/remote";
    let token = store.selected_desired_token(subject).unwrap().unwrap();
    assert_eq!(
        st3::agent_launch::read(&store, subject, &token)
            .unwrap()
            .stage,
        "replicating"
    );
    assert_eq!(
        st3::agent_launch::read(&Store::open_memory("workshop").unwrap(), subject, &token)
            .unwrap()
            .stage,
        "replicating"
    );
}

#[test]
fn real_crash_does_not_restart_on_repeated_reconcile_passes_and_new_revision_bypasses_delay() {
    if st3::test_support::supervise_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("studio").unwrap());
    let runtime = Arc::new(Processes::new(root.path(), 9));
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "studio".into(),
        Arc::new(Notify::new()),
    );
    let source = format!(
        "version 2\nagent \"example/crash\" {{ workspace {:?}; command \"true\"; restart always; }}",
        root.path()
    );
    declare(&store, &source, "initial");
    for _ in 0..12 {
        reconciler.reconcile_once().unwrap();
    }
    assert_eq!(*runtime.starts.lock().unwrap(), 1);
    let decision = store
        .latest_observation("agent/example/crash", "runtime.reconcile-decision")
        .unwrap()
        .unwrap();
    assert!(
        decision.body["fields"]["key"]
            .as_str()
            .unwrap()
            .starts_with("crash-backoff:")
    );
    let due = decision.body["fields"]["restart_at_unix_ms"]
        .as_str()
        .unwrap()
        .parse::<u128>()
        .unwrap();
    assert!(due >= decision.accepted_at_unix_ms + 4_000);
    declare(
        &store,
        &source.replace("command \"true\"", "command \"false\""),
        "replacement",
    );
    reconciler.reconcile_once().unwrap();
    assert_eq!(*runtime.starts.lock().unwrap(), 2);
}
