//! Start-preemption test that serves the API; it lives in st3-daemon because the API does.

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
use tokio::sync::mpsc;
use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};
use std::io::Write as _;
use tokio::io::AsyncReadExt as _;


fn sleep_program() -> String {
std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
    .map(|directory| directory.join("sleep"))
    .find(|candidate| candidate.is_file())
    .map(|candidate| candidate.display().to_string())
    .unwrap_or_else(|| "/bin/sleep".into())
}


struct NativeLatePty {
root: tempfile::TempDir,
store: Arc<Store>,
runtime: Arc<NativeRuntime>,
reconciler: Reconciler<NativeRuntime>,
desired: DesiredSubject,
real_pty: std::path::PathBuf,
gate: std::fs::File,
ready: tokio::net::UnixListener,
}

impl NativeLatePty {
fn new() -> Self {
    use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};
    st_runtime::initialize_isolation(&BTreeMap::new());
    let real_pty =
        st_runtime::resolve_executable("pty", &crate::environment::snapshot().unwrap())
            .unwrap();
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let pty = bin.join("pty");
    std::fs::write(pty.with_extension("real"), real_pty.as_os_str().as_bytes()).unwrap();
    let gate_path = pty.with_extension("gate");
    let c_path = std::ffi::CString::new(gate_path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate_path)
        .unwrap();
    let ready = tokio::net::UnixListener::bind(pty.with_extension("event")).unwrap();
    std::fs::write(&pty, r#"#!/bin/sh
(
  IFS= read -r permitted < "$0.gate" || exit 1
  "$(cat "$0.real")" "$@"
  outcome=$?
  python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.connect(sys.argv[1]); s.sendall(bytes([int(sys.argv[2])]))' "$0.event" "$outcome"
) </dev/null >"$0.output" 2>&1 &
exit 1
"#).unwrap();
    std::fs::set_permissions(&pty, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let store = Arc::new(Store::open(&root.path().join("graph.db"), "node").unwrap());
    apply_source(
        &store,
        &format!(
            "version 2\nagent \"example/late\" {{ workspace {:?}; argv {:?} \"1000\"; }}",
            root.path().display().to_string(),
            sleep_program()
        ),
        "late-native",
    );
    let desired = store
        .desired_subjects_named(&["agent/example/late".into()])
        .unwrap()
        .remove(0);
    let runtime = Arc::new(NativeRuntime::new(&state, None, &pty));
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    Self {
        root,
        store,
        runtime,
        reconciler,
        desired,
        real_pty,
        gate,
        ready,
    }
}

fn dispatch(&self) -> Result<bool> {
    let member = self.desired.member.as_ref().unwrap().clone();
    let mut launch_member = member.clone();
    launch_member.environment.insert(
        "PATH".into(),
        format!("{}:${{PATH}}", self.root.path().join("bin").display()),
    );
    self.reconciler.dispatch_start(st3_reconcile::reconcile::start_jobs::PreparedStart {
        subject: self.desired.clone(),
        member,
        launch_member,
        reason: "native post-producer failure regression".into(),
        request: None,
        desired_token: self.reconciler.launch_token(&self.desired.subject)?,
        selected_token: self.store.selected_desired_token(&self.desired.subject)?,
        placement_evidence: Vec::new(),
        start_request: String::new(),
        start_request_receipt: None,
        control_request: None,
        control_request_receipt: None,
        expected_incarnation: None,
        resume_operation: None,
    })
}

fn assert_open(&self) -> crate::store::RuntimeStartOperation {
    let operation = self
        .store
        .runtime_start_for(&self.desired.subject)
        .unwrap()
        .unwrap();
    assert!(operation.finished && operation.revoked_by.is_some());
    assert!(!operation.cleanup_verified);
    assert_eq!(operation.result.as_ref().unwrap()["producer_closed"], false);
    assert_eq!(self.store.runtime_start_counts().unwrap(), (0, 0));
    assert_eq!(
        self.store.unresolved_runtime_start_counts().unwrap(),
        (1, 1)
    );
    assert!(
        !self.dispatch().unwrap(),
        "a second start escaped the unresolved producer"
    );
    self.reconciler.reconcile_start_jobs().unwrap();
    assert!(
        self.store
            .observations_for(&self.desired.subject, "harness.diagnostic")
            .unwrap()
            .iter()
            .any(|claim| claim.body["fields"]["code"] == "start-revocation-unverified")
    );
    operation
}

}

#[tokio::test]
async fn native_returned_pty_error_retains_unknown_and_visible_open_count() {
if crate::test_support::supervise_test() {
    return;
}
use tower::ServiceExt as _;
let fixture = NativeLatePty::new();
assert!(fixture.dispatch().unwrap());
let operation = fixture.assert_open();
assert!(fixture.runtime.snapshot_ptys().unwrap().is_empty());
let state = fixture.root.path().join("state");
let app = crate::api::router(crate::api::AppState {
    owning_person: None,
    store: fixture.store.clone(),
    notify: Arc::new(Notify::new()),
    event_notify: tokio::sync::watch::channel(0).0,
    node: "node".into(),
    pty_root: state.join("pty"),
    state_dir: state,
    pty_binary: fixture.real_pty.clone(),
    fleet_id: None,
    configured_peers: Vec::new(),
    client_relay: None,
    native_session_home: None,
    planner_default: crate::model::PlannerSpec::default(),
});
let response = app
    .oneshot(
        axum::http::Request::builder()
            .uri("/v1/client/agents/agent/example/late")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
assert_eq!(response.status(), axum::http::StatusCode::OK);
let card: Value = serde_json::from_slice(
    &axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap(),
)
.unwrap();
assert_eq!(card["value"]["runtime_start"]["operation_id"], operation.id);
assert_eq!(card["value"]["runtime_start"]["unresolved_jobs"], 1);
assert_eq!(card["value"]["runtime_start"]["revoked_unresolved_jobs"], 1);
assert_eq!(card["value"]["runtime_start"]["worker_running"], false);
assert_eq!(card["value"]["runtime_start"]["cleanup_verified"], false);
let failed = fixture
    .store
    .observations_for(&fixture.desired.subject, "runtime.action.failed")
    .unwrap()
    .pop()
    .unwrap();
assert!(failed.body["fields"]["operation_status"].is_null());
assert_eq!(failed.body["fields"]["operation"], operation.id);
}

