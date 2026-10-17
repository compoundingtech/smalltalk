//! Gate-on-demand test that serves the API; it lives in st3-daemon because the API does.

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


const SOURCE: &str = r#"version 2
mission "orchid" state="ready" {
  goal "Refine a garden."
  loop "refine" {
max-rounds 2
round {
  completion { when "all-steps-exhausted" }
  agent "builder" { workspace "/tmp"; command "true"; restart "never" }
  agent "gate" { workspace "/tmp"; command "true"; restart "never" }
  agent "helper" { workspace "/tmp"; command "true"; restart "never" }
  step "build" { assigned-to "agent/${ST_MISSION_RUN}/builder" }
  step "gate" {
    assigned-to "agent/${ST_MISSION_RUN}/gate"
    depends-on { step "build" completed }
    retry { attempts 2 }
  }
}
  }
}"#;

struct Fixture {
store: Arc<Store>,
runtime: Arc<FakeRuntime>,
reconciler: Reconciler<FakeRuntime>,
round: MissionRunView,
}

impl Fixture {
fn new(source: &str) -> Self {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(&store, source, "publish");
    let parent = store
        .create_mission_run(&crate::model::MissionRunRequest {
            mission: "orchid".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/lichen".into()),
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: "garden".into(),
        })
        .unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    Self::passes(&reconciler, &runtime);
    let child = store.mission_run_subject_for_idempotency_key(&format!(
        "loop-round:{}:1",
        parent.steps[0].subject
    ));
    let round = store.mission_run(&child).unwrap().unwrap();
    let fixture = Self {
        store,
        runtime,
        reconciler,
        round,
    };
    fixture.settle();
    fixture
}

fn settle(&self) {
    Self::passes(&self.reconciler, &self.runtime);
}

fn passes(reconciler: &Reconciler<FakeRuntime>, runtime: &FakeRuntime) {
    for _ in 0..8 {
        let members = runtime.started_members.lock().unwrap().clone();
        let stops = runtime.stops.lock().unwrap().clone();
        let mut ptys = runtime.ptys.lock().unwrap();
        for observation in ptys.iter_mut() {
            if stops.contains(&observation.runtime_id) {
                observation.status = "stopped".into();
            }
        }
        for member in members {
            if !stops.contains(&member.runtime_id)
                && !ptys.iter().any(|p| p.runtime_id == member.runtime_id)
            {
                ptys.push(RuntimeObservation {
                    runtime_id: member.runtime_id.clone(),
                    terminal: true,
                    status: "running".into(),
                    exit_code: None,
                    incarnation_id: Some(format!("{}-one", member.runtime_id)),
                });
            }
        }
        drop(ptys);
        reconciler.reconcile_once().unwrap();
    }
}

fn step(&self, path: &str) -> crate::model::StepRunView {
    self.store
        .mission_run(&self.round.id)
        .unwrap()
        .unwrap()
        .steps
        .into_iter()
        .find(|s| s.step == path)
        .unwrap()
}

fn action(&self, path: &str, action: &str, key: &str) -> crate::model::StepRunView {
    let step = self.step(path);
    let actor = step.assigned_to.unwrap();
    let member = self
        .runtime
        .started_members
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.environment.get("ST_AGENT").map(String::as_str) == Some(actor.as_str()))
        .unwrap()
        .clone();
    self.store
        .work_action(
            &step.subject,
            action,
            &crate::model::WorkRequest {
                wait_ms: None,
                actor: Some(actor),
                incarnation: Some(format!("{}-one", member.runtime_id)),
                summary: Some("The work is done.".into()),
                reason: Some("Try another review.".into()),
                evidence: Vec::new(),
                idempotency_key: key.into(),
            },
        )
        .unwrap()
}

fn gate_starts(&self) -> usize {
    let gate = format!("agent/{}/gate", self.round.id);
    self.runtime
        .started_members
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.environment.get("ST_AGENT").map(String::as_str) == Some(gate.as_str()))
        .count()
}

fn finish_build(&self) {
    self.action("build", "claim", "claim-build");
    self.action("build", "complete", "complete-build");
    self.settle();
}

fn restart(&mut self) {
    self.reconciler = Reconciler::new(
        self.store.clone(),
        self.runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    self.settle();
}

fn revise(&self, key: &str) {
    let source = SOURCE.replace(
        "      agent \"helper\"",
        &format!(
            "      resource \"{key}\" {{ kind \"custom.test.garden\" }}\n      agent \"helper\""
        ),
    );
    apply_source(&self.store, &source, key);
    let next = self
        .store
        .mission_spec("__st3/orchid/loop/refine/round", None)
        .unwrap()
        .unwrap();
    self.store
        .adopt_mission_revision(
            &self.round.id,
            &next,
            "person/lichen",
            "Add garden evidence.",
            &format!("adopt-{key}"),
        )
        .unwrap();
    self.settle();
}
}

#[tokio::test]
async fn gate_on_demand_builder_message_waits_in_the_gate_mailbox() {
let fixture = Fixture::new(SOURCE);
let gate = format!("agent/{}/gate", fixture.round.id);
use tower::ServiceExt;
let root = tempfile::tempdir().unwrap();
let state = crate::api::AppState {
    owning_person: None,
    store: fixture.store.clone(),
    notify: Arc::new(Notify::new()),
    event_notify: tokio::sync::watch::channel(0_u64).0,
    node: "node".into(),
    state_dir: root.path().into(),
    pty_root: root.path().join("pty"),
    pty_binary: "pty".into(),
    fleet_id: None,
    configured_peers: Vec::new(),
    client_relay: None,
    native_session_home: None,
    planner_default: Default::default(),
};
let response = crate::api::router(state)
    .oneshot(
        axum::http::Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&crate::model::MessageSendRequest {
                    idempotency_key: "garden-handoff".into(),
                    from: format!("agent/{}/builder", fixture.round.id),
                    to: gate.clone(),
                    content: "Review the garden.".into(),
                    title: None,
                    in_reply_to: None,
                    tags: Vec::new(),
                    attachments: Vec::new(),
                })
                .unwrap(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
assert_eq!(response.status(), axum::http::StatusCode::OK);
let body = axum::body::to_bytes(response.into_body(), usize::MAX)
    .await
    .unwrap();
let envelope: Value = serde_json::from_slice(&body).unwrap();
let message: crate::model::MessageView =
    serde_json::from_value(envelope["value"].clone()).unwrap();
fixture.settle();
assert_eq!(
    fixture.gate_starts(),
    0,
    "mail alone does not admit the gate"
);
assert_eq!(
    fixture.store.messages(Some(&gate), false).unwrap()[0].content,
    "Review the garden."
);
fixture.finish_build();
assert_eq!(fixture.gate_starts(), 1);
assert!(
    fixture
        .store
        .messages(Some(&gate), false)
        .unwrap()
        .iter()
        .any(|m| m.subject == message.subject && m.status == "sent")
);
}
