//! First-readiness tests that serve the API; they live in st3-daemon because the API does.

#![allow(unused_imports, dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use serde_json::{Value, json};
use crate::model::*;
use crate::store::Store;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use anyhow::{Context as _};
use chrono::{Datelike as _, NaiveDate, TimeZone as _, Utc};
use notify::Watcher as _;
use sha2::Digest as _;
use tokio::sync::{Notify, watch};
use crate::checkout::Checkout;
use crate::mission::{ CANDIDATE_INDEX_INPUT, LOOP_FEEDBACK_INPUT, LOOP_ITEM_INPUT, LOOP_ROUND_INPUT, };
use crate::model::{ AttentionItemView, AttentionRequest, CalendarSchedule, ClaimInput, CurrentHarnessView, DependencySpec, DesiredSubject, GateContext, GateSpec, LaunchSpec, LoopCandidateSelector, LoopExhaustionSpec, LoopSpec, MemberKind, MemberLifecycle, MemberSpec, MessageView, MetricSource, MissionInputKind, MissionRunRequest, MissionRunView, MissionSpec, MissionState, RestartIntensity, RestartType, StepRunView, StepSpec, SubscriptionSpec, UsedMissionSpec, WorkSelector, };
use crate::resource::{ ObservationRequest, ObserverCondition, ProviderRateLimit, RegisteredResourceProvider, ResourceProvider, };
use st3_reconcile::reconcile::idle_nudge::{IDLE_NUDGE_AFTER_MS, IDLE_NUDGE_QUIET_WATCH_MS, IdleNudgeSettings};
use crate::model::FIRST_READINESS_FAULT_SCOPE;
use std::os::unix::fs::PermissionsExt as _;
use crate::suspension as suspended;
use st3_reconcile::reconcile::overlay_runtime_environment;
use st_runtime::PRIORITY_ENV;
use st3_reconcile::reconcile::{NativeRuntime, RuntimeControl};
use st3_reconcile::reconcile::*;
use st3_reconcile::reconcile::tests::*;
use crate::client::{Client, Endpoint};
use std::time::Instant;

use crate::api::{AppState, router, serve_unix_with_ready};

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
    owning_person: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partially_progressed_run_exposes_its_scheduler_wait_without_a_writer() {
let root = tempfile::tempdir().unwrap();
let store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
let source = SOURCE.replace("${ST_WORKSPACE}", &root.path().display().to_string())
    + r#"
mission "progressed" state="ready" {
  goal "Explain readiness after some work completes."
  step "start" { agentless }
  step "next" {
assigned-to "agent/orchid/worker"
depends-on { step "start" completed }
  }
  step "finish" {
agentless
depends-on { step "next" completed }
  }
}
"#;
apply_source(&store, &source, "partial-scheduler-wait");
let run = start(&store, "progressed", "progressed", root.path());
let first = run.steps.iter().find(|step| step.step == "start").unwrap();
let next = run
    .steps
    .iter()
    .find(|step| step.step == "next")
    .unwrap()
    .subject
    .clone();
store
    .set_step_state(&first.subject, "completed", None)
    .unwrap();
assert_eq!(
    store.step_run(&first.subject).unwrap().unwrap().status,
    "completed"
);
assert_eq!(store.step_run(&next).unwrap().unwrap().readiness_epoch, 0);
assert!(
    store
        .reconcile_fault(&run.subject, FIRST_READINESS_FAULT_SCOPE)
        .unwrap()
        .is_none()
);

let notify = Arc::new(Notify::new());
let state = AppState {
    owning_person: None,
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
ready.await.unwrap();
let client = Client::new(Endpoint::Unix(socket));

// Saturation can hold the writer needed for promotion. The read must still explain
// the scheduler's ownership without waiting for that writer or inventing readiness.
let (entered, holding) = std::sync::mpsc::channel();
let (release, released) = std::sync::mpsc::channel();
let writer_store = store.clone();
let writer = std::thread::spawn(move || {
    writer_store.hold_writer_for_test(|| {
        entered.send(()).unwrap();
        released.recv().unwrap();
    });
});
holding.recv_timeout(Duration::from_secs(5)).unwrap();
let response = tokio::time::timeout(
    Duration::from_secs(5),
    client.get::<Value>(&format!("/v1/client/work/{next}")),
)
.await;
release.send(()).unwrap();
writer.join().unwrap();
let response = response.unwrap().unwrap();
let waiting = &response;
assert_eq!(waiting["state"], "waiting", "{response}");
assert_eq!(waiting["readiness_epoch"], 0);
assert_eq!(waiting["operational"]["actionable"], false);
assert!(
    waiting["operational"]["reasons"]
        .as_array()
        .is_some_and(|reasons| {
            reasons.iter().any(|reason| {
                reason
                    .as_str()
                    .is_some_and(|reason| reason.contains("daemon/runtime"))
            })
        }),
    "Satisfied dependent step has no named scheduler waiter: {waiting}"
);
let step = store.step_run(&next).unwrap().unwrap();
let single = store.work_annotation(&step).unwrap();
let batch = store.work_annotations(std::slice::from_ref(&step)).unwrap();
assert_eq!(
    serde_json::to_value(single).unwrap(),
    serde_json::to_value(&batch[&next]).unwrap()
);

let reconciler = Reconciler::new(
    store.clone(),
    Arc::new(FakeRuntime::default()),
    "node".into(),
    notify,
);
for _ in 0..3 {
    reconciler.evaluate_mission_runs().unwrap();
}
let promoted: Value = client
    .get(&format!("/v1/client/work/{next}"))
    .await
    .unwrap();
assert_eq!(promoted["state"], "ready");
assert_eq!(promoted["operational"]["actionable"], true);
assert!(
    promoted["operational"]["reasons"]
        .as_array()
        .is_none_or(|reasons| reasons.is_empty())
);
store
    .request_mission_run_cancellation(&run.id, "test completed")
    .unwrap();
let cancelled: Value = client
    .get(&format!("/v1/client/work/{next}"))
    .await
    .unwrap();
assert_eq!(cancelled["state"], "cancelled");
assert!(
    !cancelled["operational"]["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| {
            reason
                .as_str()
                .is_some_and(|reason| reason.contains("daemon/runtime"))
        })
);
server.abort();
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
