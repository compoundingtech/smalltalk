use super::*;
use crate::api::{AppState, router, serve_unix_with_ready};
use crate::client::{Client, Endpoint};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

struct Clock;
impl Clock {
    fn at(at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        Self
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

fn start(store: &Store, mission: &str, key: &str, workspace: &Path) -> MissionRunView {
    store
        .create_mission_run(&MissionRunRequest {
            mission: mission.into(),
            revision: None,
            workspace: workspace.display().to_string(),
            requester: Some("agent/orchid/worker".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: key.into(),
        })
        .unwrap()
}

const SOURCE: &str = r#"version 2
agent "orchid/worker" { workspace "${ST_WORKSPACE}"; command "true"; restart "never" }
resource "marker" { kind "custom.st3.document-source" }
mission "gated" state="ready" {
  concurrent-runs
  goal "Continue checking beside newly admitted work."
  step "check" {
    agentless
    gate "marker holds" { field "state" "resource/marker" is "ready" }
  }
}
mission "fresh" state="ready" {
  concurrent-runs
  goal "Offer first readiness promptly."
  step "work" { assigned-to "agent/orchid/worker" }
}
"#;

struct SlowGateWriter {
    store: Arc<Store>,
    first: AtomicBool,
    entered: std::sync::mpsc::Sender<()>,
}

impl FaultInjection for SlowGateWriter {
    fn fault(&self, scope: &str, _: &str) -> Option<String> {
        if scope == "gate-write" && !self.first.swap(true, Ordering::SeqCst) {
            // Lend the actual isolated daemon writer at the gate-result append boundary.
            // Readers remain available; the serial evaluator is waiting for the gate writer.
            self.store.hold_writer_for_test(|| {
                self.entered.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1_500));
            });
        }
        None
    }
}

async fn isolated_daemon_first_readiness(old_order: bool, skip_unneeded: bool) -> bool {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    // Expand workspace only for the host declaration, as normal publication does.
    apply_source(
        &store,
        &SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string()),
        "publish",
    );
    store
        .append_claim(&ClaimInput {
            subject: "resource/marker".into(),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([("state".into(), Value::String("ready".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("marker-ready".into()),
        })
        .unwrap();
    let mut creation_order = Vec::new();
    let mut older = Vec::new();
    for index in 0..32 {
        let run = start(&store, "gated", &format!("older-{index}"), root.path());
        store
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap();
        creation_order.push(run.id.clone());
        older.push(run);
    }
    let fresh = start(&store, "fresh", "new-first-readiness", root.path());
    creation_order.push(fresh.id.clone());
    let notify = Arc::new(Notify::new());
    let state = AppState {
        store: store.clone(),
        notify: notify.clone(),
        event_notify: tokio::sync::watch::channel(0).0,
        node: "node".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: "unused-pty".into(),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let (listening, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        serve_unix_with_ready(&server_socket, router(state), || {
            let _ = listening.send(());
        })
        .await
        .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    let client = Client::new(Endpoint::Unix(socket));
    let (entered, slow_writer) = std::sync::mpsc::channel();
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        notify,
    )
    .skipping_unneeded(skip_unneeded)
    .with_fault_injection(Arc::new(SlowGateWriter {
        store: store.clone(),
        first: AtomicBool::new(false),
        entered,
    }));
    let evaluator = tokio::task::spawn_blocking(move || {
        if old_order {
            // Same evaluator and writer, with an injected oldest-first creation list.
            // This negative control models the former order; it does not run the former SQL.
            reconciler.incremental.observe(&reconciler.store).unwrap();
            reconciler.evaluate_mission_run_ids(creation_order).unwrap();
        } else {
            reconciler.evaluate_mission_runs().unwrap();
        }
    });
    tokio::task::spawn_blocking(move || slow_writer.recv_timeout(Duration::from_secs(5)).unwrap())
        .await
        .unwrap();
    let health: Value = tokio::time::timeout(Duration::from_millis(750), client.get("/v1/health"))
        .await
        .unwrap()
        .unwrap();
    assert!(health.is_object());
    let started = Instant::now();
    let ready = tokio::time::timeout(Duration::from_millis(750), async {
        loop {
            let run: MissionRunView = client
                .get(&format!("/v1/mission-runs/{}", fresh.id))
                .await
                .unwrap();
            if run.steps[0].status == "ready" {
                assert_eq!(run.steps[0].readiness_epoch, 1);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let readiness_elapsed = started.elapsed();
    evaluator.await.unwrap();
    // Priority does not skip older evaluations, nor admit the original step twice.
    for run in older {
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "completed"
        );
    }
    let states = store
        .claims_for(&fresh.steps[0].subject, Some("step-run.state"))
        .unwrap();
    assert_eq!(
        states
            .iter()
            .filter(|claim| claim.body["fields"]["status"] == "ready")
            .count(),
        1
    );
    eprintln!(
        "first readiness: old_order={old_order}, incremental={skip_unneeded}, ready_within_750ms={ready}, observed={:?}",
        readiness_elapsed
    );
    server.abort();
    ready
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_readiness_bypasses_slow_gate_writes_on_an_isolated_daemon() {
    assert!(
        !isolated_daemon_first_readiness(true, false).await,
        "the previous order must fail the same readiness bound"
    );
    for incremental in [false, true] {
        assert!(
            isolated_daemon_first_readiness(false, incremental).await,
            "first readiness must precede the old gate writer"
        );
    }
}

#[test]
fn first_readiness_fault_is_once_visible_and_recovers() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    apply_source(
        &store,
        &SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string()),
        "publish",
    );
    let run = start(&store, "fresh", "delayed", root.path());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let since = run.steps[0].created_at_unix_ms.max(run.created_at_unix_ms);
    reconciler
        .record_first_readiness_wait(&run, since + FIRST_READINESS_FAULT_AFTER_MS - 1)
        .unwrap();
    assert!(
        store
            .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_none()
    );
    // Repeated observations and daemon reconstruction preserve one stable cause.
    for offset in [0, 1, 50_000] {
        reconciler
            .record_first_readiness_wait(&run, since + FIRST_READINESS_FAULT_AFTER_MS + offset)
            .unwrap();
    }
    let restarted = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    restarted
        .record_first_readiness_wait(&run, since + 2 * FIRST_READINESS_FAULT_AFTER_MS)
        .unwrap();
    let faults = store
        .claims_for(&run.subject, Some("reconcile.fault"))
        .unwrap();
    assert_eq!(faults.len(), 1);
    assert_eq!(
        faults[0].body["fields"]["scope"],
        FIRST_READINESS_FAULT_SCOPE
    );
    let reason = store
        .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
        .unwrap()
        .unwrap();
    assert!(
        reason.contains(&run.subject) && reason.contains("120000ms"),
        "{reason}"
    );
    assert_eq!(
        store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .scheduler_fault
            .as_deref(),
        Some(reason.as_str())
    );
    assert_eq!(
        store
            .mission_run_steps(&run.id, true)
            .unwrap()
            .unwrap()
            .scheduler_fault
            .as_deref(),
        Some(reason.as_str())
    );
    let items = store.fault_snapshot(now_ms()).unwrap();
    assert!(
        items.iter().all(|fault| fault.item.subject != run.subject),
        "{items:?}"
    );
    restarted.evaluate_mission_runs().unwrap();
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "ready"
    );
    restarted.evaluate_mission_runs().unwrap();
    assert!(
        store
            .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .scheduler_fault
            .is_none()
    );
    assert!(
        store
            .fault_snapshot(now_ms())
            .unwrap()
            .iter()
            .all(|fault| fault.item.subject != run.subject)
    );
    let faults = store
        .claims_for(&run.subject, Some("reconcile.fault"))
        .unwrap();
    assert_eq!(
        faults
            .iter()
            .map(|claim| claim.body["fields"]["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["faulted", "recovered"]
    );
}

#[test]
fn first_readiness_fault_uses_real_readiness_predicates() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        &SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string()),
        "publish",
    );
    let run = start(&store, "fresh", "late-eligible", root.path());
    // The thread clock travels without modifying production or sleeping two minutes.
    let late = run.steps[0].created_at_unix_ms + FIRST_READINESS_FAULT_AFTER_MS;
    let _clock = Clock::at(late);
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.evaluate_mission_runs().unwrap();
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "ready"
    );
    assert!(
        store
            .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_some()
    );
}

struct FailedReadinessFaultWriter {
    store: Arc<Store>,
    step: String,
    attempted: AtomicBool,
}

impl FaultInjection for FailedReadinessFaultWriter {
    fn fault(&self, scope: &str, _: &str) -> Option<String> {
        if scope != "first-readiness-fault-write" {
            return None;
        }
        // Both diagnosis and recovery must run after the ready claim is durable.
        let states = self
            .store
            .claims_for(&self.step, Some("step-run.state"))
            .unwrap();
        assert!(
            states
                .iter()
                .any(|claim| claim.body["fields"]["status"] == "ready")
        );
        self.attempted.store(true, Ordering::SeqCst);
        Some("injected scheduler diagnostic append failure".into())
    }
}

#[test]
fn first_readiness_fault_write_failure_preserves_admission_and_recovery() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        &SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string()),
        "publish",
    );
    let run = start(&store, "fresh", "failed-diagnostic", root.path());
    let _clock = Clock::at(run.steps[0].created_at_unix_ms + FIRST_READINESS_FAULT_AFTER_MS);
    let failing = Arc::new(FailedReadinessFaultWriter {
        store: store.clone(),
        step: run.steps[0].subject.clone(),
        attempted: AtomicBool::new(false),
    });
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_fault_injection(failing.clone());
    reconciler.evaluate_mission_runs().unwrap();
    assert!(failing.attempted.swap(false, Ordering::SeqCst));
    let admitted = store.mission_run(&run.id).unwrap().unwrap();
    assert_eq!(
        (
            admitted.steps[0].status.as_str(),
            admitted.steps[0].readiness_epoch
        ),
        ("ready", 1)
    );
    assert!(
        store
            .claims_for(&run.subject, Some("reconcile.fault"))
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .claims_for(&run.steps[0].subject, Some("reconcile.fault"))
            .unwrap()
            .is_empty()
    );

    // Seed a persisted open diagnosis, then fail its recovery append on the next pass.
    let healthy = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    healthy.record_first_readiness_wait(&run, now_ms()).unwrap();
    let restarted = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .with_fault_injection(failing.clone());
    restarted.evaluate_mission_runs().unwrap();
    assert!(failing.attempted.load(Ordering::SeqCst));
    let faults = store
        .claims_for(&run.subject, Some("reconcile.fault"))
        .unwrap();
    assert_eq!(faults.len(), 1);
    assert_eq!(
        faults[0].body["fields"]["scope"],
        FIRST_READINESS_FAULT_SCOPE
    );
    assert_eq!(
        store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
        "ready"
    );
    let states = store
        .claims_for(&run.steps[0].subject, Some("step-run.state"))
        .unwrap();
    assert_eq!(
        states
            .iter()
            .filter(|claim| claim.body["fields"]["status"] == "ready")
            .count(),
        1
    );
    healthy.evaluate_mission_runs().unwrap();
    assert!(
        store
            .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_none()
    );
}

#[test]
fn first_readiness_after_restart_does_not_page_for_each_late_run() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        &SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string()),
        "publish",
    );
    let runs = (0..32)
        .map(|index| start(&store, "fresh", &format!("late-{index}"), root.path()))
        .collect::<Vec<_>>();
    let _clock = Clock::at(now_ms() + FIRST_READINESS_FAULT_AFTER_MS);
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.evaluate_mission_runs().unwrap();
    // A reconstructed daemon loads the persisted open faults rather than emitting new ones.
    let restarted = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    for run in &runs {
        restarted
            .record_first_readiness_wait(run, now_ms())
            .unwrap();
        assert_eq!(
            store
                .claims_for(&run.subject, Some("reconcile.fault"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .mission_run(&run.id)
                .unwrap()
                .unwrap()
                .scheduler_fault
                .is_some()
        );
    }
    // Ordinary reconciliation errors retain their existing operator attention behavior.
    restarted
        .record_fault(
            "daemon/node",
            "other",
            Err(anyhow::anyhow!("unrelated failure")),
        )
        .unwrap();
    let items = store
        .fault_snapshot(now_ms() + FIRST_READINESS_FAULT_AFTER_MS)
        .unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].item.subject, "daemon/node");
}

#[test]
fn first_readiness_newest_child_order_advances_nested_runs() {
    for incremental in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
resource "finished" { kind "custom.st3.document-source" }
mission "parent" state="ready" {
  goal "Advance a nested round before finishing its parent."
  loop "round" {
    max-rounds 1
    until { gate "done" { field "state" "resource/finished" is "ready" } }
    round {
      completion { when "all-steps-exhausted" }
      step "work" { agentless }
    }
  }
}"#,
            "nested-order",
        );
        let parent = start(&store, "parent", "nested-parent", root.path());
        let _clock = Clock::at(now_ms() + 10);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .skipping_unneeded(incremental);
        reconciler.evaluate_mission_runs().unwrap();
        reconciler.evaluate_mission_runs().unwrap();
        let child_subject = store.mission_run_subject_for_idempotency_key(&format!(
            "loop-round:{}:1",
            parent.steps[0].subject
        ));
        let child = store.mission_run(&child_subject).unwrap().unwrap();
        assert_eq!(
            child.parent_step_run.as_deref(),
            Some(parent.steps[0].subject.as_str())
        );
        assert_eq!(
            store.active_mission_run_ids_for_origin("node").unwrap(),
            [child.id.clone(), parent.id.clone()]
        );
        reconciler.evaluate_mission_runs().unwrap();
        assert_eq!(
            store.mission_run(&child.id).unwrap().unwrap().steps[0].status,
            "ready"
        );
        assert_eq!(
            store.mission_run(&parent.id).unwrap().unwrap().steps[0].status,
            "working"
        );
        // Both runs are now in progress: newest-first still places the child first.
        assert_eq!(
            store.active_mission_run_ids_for_origin("node").unwrap(),
            [child.id.clone(), parent.id.clone()]
        );
        store
            .append_claim(&ClaimInput {
                subject: "resource/finished".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), Value::String("ready".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("round-finished".into()),
            })
            .unwrap();
        // Parent fences and child completion remain valid when children are visited first.
        for _ in 0..8 {
            reconciler.evaluate_mission_runs().unwrap();
        }
        for run in [&parent, &child] {
            let completed = store.mission_run(&run.id).unwrap().unwrap();
            assert_eq!(
                completed.status, "completed",
                "incremental={incremental}: {completed:?}"
            );
            assert_eq!(completed.steps[0].readiness_epoch, 1);
            assert!(completed.scheduler_fault.is_none());
        }
    }
}

#[test]
fn first_readiness_fault_excludes_predicate_blockers_and_new_generations() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_memory("node").unwrap());
    let source = format!(
        r#"version 2
agent "orchid/worker" {{ workspace "{}"; command "true"; restart "never" }}
resource "marker" {{ kind "custom.st3.document-source" }}
mission "mission-baseline" state="ready" {{
  goal "Wait for the mission baseline."
  baseline "marker" {{ field "state" "resource/marker" is "ready" }}
  step "work" {{ agentless }}
}}
mission "step-baseline" state="ready" {{
  goal "Wait for the step baseline."
  step "work" {{ agentless; baseline "marker" {{ field "state" "resource/marker" is "ready" }} }}
}}
mission "assignment" state="ready" {{
  goal "Wait for an eligible declaration."
  step "work" {{ assigned-to "agent/orchid/missing" }}
}}
mission "dependencies" state="ready" {{
  goal "Wait for the dependency."
  step "first" {{ assigned-to "agent/orchid/missing" }}
  step "second" {{ agentless; depends-on "first" }}
}}
mission "backoff" state="ready" {{
  goal "Wait for retry time."
  step "work" {{ assigned-to "agent/orchid/worker" }}
}}
"#,
        root.path().display()
    );
    apply_source(&store, &source, "blocked-predicates");
    let runs = [
        "mission-baseline",
        "step-baseline",
        "assignment",
        "dependencies",
        "backoff",
    ]
    .map(|mission| start(&store, mission, mission, root.path()));
    let backoff = &runs[4];
    store
        .set_step_state(&backoff.steps[0].subject, "failed", Some("retry later"))
        .unwrap();
    store
        .retry_step(
            &backoff.steps[0].subject,
            "retry later",
            4 * FIRST_READINESS_FAULT_AFTER_MS as u64,
        )
        .unwrap();
    let late = now_ms() + FIRST_READINESS_FAULT_AFTER_MS;
    let _clock = Clock::at(late);
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.evaluate_mission_runs().unwrap();
    for run in &runs {
        assert!(
            store
                .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
                .unwrap()
                .is_none(),
            "{}",
            run.subject
        );
        assert!(
            store
                .mission_run(&run.id)
                .unwrap()
                .unwrap()
                .steps
                .iter()
                .all(|step| step.status != "ready")
        );
    }
    let mut revised = backoff.clone();
    revised.created_at_unix_ms = 0;
    revised.steps[0].created_at_unix_ms = late;
    reconciler
        .record_first_readiness_wait(&revised, late)
        .unwrap();
    assert!(
        store
            .reconcile_fault(&revised.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_none()
    );
    revised.steps[0].created_at_unix_ms = 0;
    revised.steps[0].readiness_epoch = 1;
    reconciler
        .record_first_readiness_wait(&revised, late)
        .unwrap();
    assert!(
        store
            .reconcile_fault(&revised.subject, FIRST_READINESS_FAULT_SCOPE)
            .unwrap()
            .is_none()
    );
}
