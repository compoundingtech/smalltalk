#![cfg(unix)]
//! One failing item never stops the other items on a host.
//!
//! Each test fails one item of the reconcile pass, either by injecting a fault where the
//! reconciler takes that item up or by giving it the graph that stopped a real host. Beside that
//! failure, a cancelled run must still stop its worker and finish cleanup, and a run started
//! afterwards must still start its worker. The failure is recorded on the item that failed, and
//! its recovery is recorded once it clears.
//!
//! Every test owns an in-memory graph and a fake runtime, and runs without a tokio runtime so
//! no observer polls a provider.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use st3::model::{ClaimInput, IntentInput, MemberSpec, MissionRunRequest, MissionRunView};
use st3::reconcile::{FaultInjection, Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use tokio::sync::Notify;

const HOST: &str = "node";
const DAEMON: &str = "daemon/node";

/// Runtimes that start as running and exit when stopped.
#[derive(Default)]
struct Runtime {
    execs: Mutex<HashMap<String, RuntimeObservation>>,
    failed_starts: Mutex<BTreeSet<String>>,
    failed_stops: Mutex<BTreeSet<String>>,
    snapshot_unavailable: std::sync::atomic::AtomicBool,
    incarnations: AtomicU64,
}

impl Runtime {
    fn running(&self, runtime_id: &str) -> bool {
        self.execs
            .lock()
            .unwrap()
            .get(runtime_id)
            .is_some_and(|exec| exec.status == "running")
    }
}

impl RuntimeControl for Runtime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        anyhow::ensure!(
            !self.snapshot_unavailable.load(Ordering::SeqCst),
            "the PTY registry did not answer"
        );
        Ok(self
            .execs
            .lock()
            .unwrap()
            .values()
            .filter(|runtime| runtime.terminal)
            .cloned()
            .collect())
    }

    fn observe_exec(&self, runtime_id: &str) -> Result<Option<RuntimeObservation>> {
        Ok(self
            .execs
            .lock()
            .unwrap()
            .get(runtime_id)
            .filter(|runtime| !runtime.terminal)
            .cloned())
    }

    fn start(&self, member: &MemberSpec) -> Result<()> {
        anyhow::ensure!(
            !self
                .failed_starts
                .lock()
                .unwrap()
                .contains(&member.runtime_id),
            "the runtime refused to start {}",
            member.runtime_id
        );
        let incarnation = self.incarnations.fetch_add(1, Ordering::SeqCst);
        self.execs.lock().unwrap().insert(
            member.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: member.terminal,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(format!("incarnation-{incarnation}")),
            },
        );
        Ok(())
    }

    fn stop(
        &self,
        runtime_id: &str,
        _terminal: bool,
        _expected_incarnation: Option<&str>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.failed_stops.lock().unwrap().contains(runtime_id),
            "the runtime refused to stop {runtime_id}"
        );
        if let Some(exec) = self.execs.lock().unwrap().get_mut(runtime_id) {
            exec.status = "exited".into();
            exec.exit_code = Some(0);
        }
        Ok(())
    }

    fn kill(
        &self,
        runtime_id: &str,
        terminal: bool,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.stop(runtime_id, terminal, expected_incarnation)
    }

    fn remove(&self, runtime_id: &str, _terminal: bool) -> Result<()> {
        self.execs.lock().unwrap().remove(runtime_id);
        Ok(())
    }

    fn screen(&self, _runtime_id: &str) -> Result<String> {
        Ok(String::new())
    }

    fn send_key(&self, _runtime_id: &str, _key: &str) -> Result<()> {
        Ok(())
    }

    fn read_exec_log(&self, _runtime_id: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

#[derive(Clone, Copy)]
enum Fault {
    Error,
    Panic,
}

/// The items to fail, by scope and subject.
#[derive(Default)]
struct Faults {
    items: Mutex<BTreeMap<(String, String), Fault>>,
}

impl Faults {
    fn fail(&self, scope: &str, subject: &str, fault: Fault) {
        self.items
            .lock()
            .unwrap()
            .insert((scope.into(), subject.into()), fault);
    }

    fn clear(&self) {
        self.items.lock().unwrap().clear();
    }
}

impl FaultInjection for Faults {
    fn fault(&self, scope: &str, subject: &str) -> Option<String> {
        let fault = self
            .items
            .lock()
            .unwrap()
            .get(&(scope.to_owned(), subject.to_owned()))
            .copied();
        match fault {
            Some(Fault::Error) => Some(format!("injected fault in {scope}")),
            Some(Fault::Panic) => panic!("injected panic in {scope}"),
            None => None,
        }
    }
}

struct Host {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    faults: Arc<Faults>,
    reconciler: Reconciler<Runtime>,
}

impl Host {
    fn new() -> Self {
        Self::with_cleanup_deadline(None)
    }

    fn with_cleanup_deadline(cleanup_deadline: Option<std::time::Duration>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = Arc::new(Store::open_memory(HOST).unwrap());
        let runtime = Arc::new(Runtime::default());
        let faults = Arc::new(Faults::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            HOST.into(),
            Arc::new(Notify::new()),
        )
        .with_fault_injection(faults.clone());
        let reconciler = match cleanup_deadline {
            Some(deadline) => reconciler.with_cleanup_deadline(deadline),
            None => reconciler,
        };
        let host = Self {
            _root: root,
            workspace,
            store,
            runtime,
            faults,
            reconciler,
        };
        host.publish(
            r#"version 2
mission "worker" state="ready" {
  goal "Keep one worker running until the run is cancelled."
  concurrent-runs max=8
  agent "worker" { workspace "${ST_WORKSPACE}"; command "sleep 600"; restart "never"; shutdown-timeout "50ms" }
  step "wait" {
    assigned-to "agent/${ST_MISSION_RUN}/worker"
    goal "Wait for cancellation."
  }
}
mission "job" state="ready" {
  goal "Run one command beside the seats."
  concurrent-runs max=8
  agent "worker" { workspace "${ST_WORKSPACE}"; command "sleep 600"; restart "never" }
  exec "task" { command "sleep 600"; restart "never" }
  step "wait" {
    assigned-to "agent/${ST_MISSION_RUN}/worker"
    goal "Wait for cancellation."
  }
}
mission "pair" state="ready" {
  goal "Offer two independent steps."
  concurrent-runs max=8
  agent "worker" { workspace "${ST_WORKSPACE}"; command "sleep 600"; restart "never" }
  step "first" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Do the first part." }
  step "second" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Do the second part." }
}"#,
            "publish-worker",
        );
        host
    }

    fn publish(&self, source: &str, key: &str) {
        let intent = st3::parse_intent(source, HOST).unwrap();
        let mission = self
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        self.store
            .apply(&intent, &mission.subject_tokens, key)
            .unwrap();
    }

    fn start(&self, key: &str) -> MissionRunView {
        self.start_mission("worker", key)
    }

    fn start_mission(&self, mission: &str, key: &str) -> MissionRunView {
        self.store
            .create_mission_run(&MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: self.workspace.to_string_lossy().into_owned(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap()
    }

    fn cancel(&self, run: &MissionRunView) {
        self.store
            .request_mission_run_cancellation(&run.id, "the operator cancelled it")
            .unwrap();
    }

    fn pass(&self, passes: usize) {
        for _ in 0..passes {
            self.reconciler.reconcile_once().unwrap();
        }
    }

    /// The runtime ID of the run's worker, read while the run still declares it.
    fn worker(&self, run: &MissionRunView) -> String {
        self.store
            .desired_subjects_for_owner_run(&run.subject)
            .unwrap()
            .into_iter()
            .filter(|desired| desired.kind == "agent")
            .find_map(|desired| desired.member)
            .map(|member| member.runtime_id)
            .unwrap_or_else(|| {
                panic!(
                    "run {} declares no worker: {:?}, declared {:?}, faults {:?}",
                    run.id,
                    self.state(run),
                    self.store
                        .desired_subjects()
                        .unwrap()
                        .iter()
                        .map(|desired| (&desired.subject, &desired.kind, &desired.owner_run))
                        .collect::<Vec<_>>(),
                    self.store.open_reconcile_faults(HOST).unwrap(),
                )
            })
    }

    /// Report the run's worker ready, as its harness driver does once it has started.
    fn ready(&self, run: &MissionRunView) {
        let agent = self.owned(run, "agent");
        let incarnation = self
            .runtime
            .execs
            .lock()
            .unwrap()
            .get(&self.worker(run))
            .and_then(|runtime| runtime.incarnation_id.clone())
            .expect("the worker is running");
        self.store
            .append_claim(&ClaimInput {
                subject: agent.clone(),
                kind: "harness.observed".into(),
                actor: Some(agent),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("pi".into())),
                    ("incarnation_id".into(), Value::String(incarnation)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    /// Whether the reconciler sent the run's worker a wake for its ready work.
    fn woken(&self, run: &MissionRunView) -> bool {
        !self
            .store
            .work_wake_messages_for_reconcile(&self.owned(run, "agent"))
            .unwrap()
            .is_empty()
    }

    /// The subject of the one declaration of `kind` that `run` owns.
    fn owned(&self, run: &MissionRunView, kind: &str) -> String {
        self.store
            .desired_subjects_for_owner_run(&run.subject)
            .unwrap()
            .into_iter()
            .find(|desired| desired.kind == kind)
            .map(|desired| desired.subject)
            .unwrap_or_else(|| panic!("run {} declares no {kind}", run.id))
    }

    /// Start a standing intake run that declares `intake` beside its own seat.
    fn start_intake(&self, intake: &str) -> MissionRunView {
        self.publish(
            &format!(
                r#"version 2
resource "repo" {{ kind "vcs.repository" }}
mission "intake" state="ready" {{
  goal "Hold intake."
  agent "seat" {{ workspace "${{ST_WORKSPACE}}"; command "sleep 600"; restart "never" }}
  step "watch" {{ assigned-to "agent/${{ST_MISSION_RUN}}/seat"; goal "Keep watching." }}
{intake}
}}"#
            ),
            "publish-intake",
        );
        let run = self.start_mission("intake", "intake");
        self.pass(2);
        run
    }

    fn state(&self, run: &MissionRunView) -> (String, String) {
        let run = self.store.mission_run(&run.id).unwrap().unwrap();
        (run.status, run.phase)
    }

    fn fault(&self, subject: &str, scope: &str) -> Option<String> {
        self.store.reconcile_fault(subject, scope).unwrap()
    }

    fn fault_records(&self, subject: &str) -> Vec<Value> {
        self.store
            .claims_for(subject, Some("reconcile.fault"))
            .unwrap()
            .into_iter()
            .map(|claim| claim.body["fields"].clone())
            .collect()
    }
}

/// Start a run and let its worker come up, so the test can cancel it beside a failure.
fn running_run(host: &Host, key: &str) -> (MissionRunView, String) {
    let run = host.start(key);
    host.pass(4);
    let worker = host.worker(&run);
    assert!(
        host.runtime.running(&worker),
        "{key} did not start its worker"
    );
    (run, worker)
}

/// With `failure` in place, a cancelled run stops its worker and finishes cleanup, and a run
/// started afterwards starts its worker and wakes it for its work.
fn assert_progress_beside(host: &Host, failure: impl FnOnce(&Host)) {
    let (cancelled, cancelled_worker) = running_run(host, "cancelled");
    failure(host);
    host.cancel(&cancelled);
    let fresh = host.start("fresh");
    host.pass(12);
    assert!(
        !host.runtime.running(&cancelled_worker),
        "the cancelled run's worker is still running"
    );
    assert_eq!(
        host.state(&cancelled),
        ("cancelled".into(), "terminal".into()),
        "the cancelled run did not finish cleanup"
    );
    assert!(
        host.runtime.running(&host.worker(&fresh)),
        "the fresh run did not start its worker"
    );
    host.ready(&fresh);
    host.pass(2);
    assert!(host.woken(&fresh), "the fresh run's worker was not woken");
}

/// Fail one stage of every pass, then clear it: the stages after it still run, and the stage
/// records one fault and then its recovery.
fn assert_stage_is_isolated(stage: &str, fault: Fault) {
    let host = Host::new();
    assert_progress_beside(&host, |host| host.faults.fail(stage, DAEMON, fault));
    let reason = host
        .fault(DAEMON, stage)
        .unwrap_or_else(|| panic!("{stage} recorded no fault"));
    assert!(reason.contains(stage), "{stage}: {reason}");
    host.faults.clear();
    host.pass(1);
    assert_eq!(host.fault(DAEMON, stage), None, "{stage} did not recover");
}

macro_rules! stage_tests {
    ($($name:ident => $stage:literal,)*) => {
        mod a_failing_stage_does_not_stop_the_stages_after_it {
            use super::*;
            $(
                mod $name {
                    use super::*;

                    #[test]
                    fn with_an_error() {
                        assert_stage_is_isolated($stage, Fault::Error);
                    }

                    #[test]
                    fn with_a_panic() {
                        assert_stage_is_isolated($stage, Fault::Panic);
                    }
                }
            )*
        }
    };
}

stage_tests! {
    intake => "stage/intake",
    observers => "stage/observers",
    schedules => "stage/schedules",
    scheduled_work => "stage/scheduled-work",
    subscriptions => "stage/subscriptions",
    provider_capacity_retries => "stage/provider-capacity-retries",
    disk_space => "stage/disk-space",
    person_asks => "stage/person-asks",
}

#[test]
fn a_failing_mission_stage_still_stops_and_starts_members() {
    let host = Host::new();
    let (cancelled, cancelled_worker) = running_run(&host, "cancelled");
    host.cancel(&cancelled);
    // One healthy pass declares the cleanup stops. The member stage carries them out even while
    // the run list cannot be read, and a new seat still starts.
    host.pass(1);
    host.faults.fail("stage/missions", DAEMON, Fault::Error);
    host.publish(
        &format!(
            "version 2\nagent \"standalone\" {{ workspace {:?}; command \"sleep 600\"; restart \"never\" }}\n",
            host.workspace
        ),
        "publish-standalone",
    );
    host.pass(4);
    assert!(!host.runtime.running(&cancelled_worker));
    let standalone = host
        .store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|desired| desired.subject == "agent/node.standalone")
        .and_then(|desired| desired.member)
        .expect("the standalone seat is declared")
        .runtime_id;
    assert!(host.runtime.running(&standalone));
    assert!(host.fault(DAEMON, "stage/missions").is_some());
    host.faults.clear();
    host.pass(4);
    assert_eq!(
        host.state(&cancelled),
        ("cancelled".into(), "terminal".into())
    );
    assert_eq!(host.fault(DAEMON, "stage/missions"), None);
}

#[test]
fn a_failing_mission_run_does_not_hold_back_other_runs() {
    for fault in [Fault::Error, Fault::Panic] {
        let host = Host::new();
        let (stuck, _) = running_run(&host, "stuck");
        assert_progress_beside(&host, |host| {
            host.faults.fail("mission-run", &stuck.subject, fault);
        });
        let reason = host
            .fault(&stuck.subject, "mission-run")
            .expect("the failing run records its fault");
        assert!(reason.contains("mission-run"), "{reason}");
        host.faults.clear();
        host.pass(1);
        assert_eq!(host.fault(&stuck.subject, "mission-run"), None);
        let records = host.fault_records(&stuck.subject);
        assert_eq!(
            records
                .iter()
                .map(|record| record["status"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["faulted", "recovered"],
            "one fault and one recovery, however many passes failed: {records:#?}"
        );
    }
}

#[test]
fn a_cancelled_run_cleans_up_while_another_run_is_failing() {
    let host = Host::new();
    let (failing, failing_worker) = running_run(&host, "failing");
    host.faults
        .fail("mission-run", &failing.subject, Fault::Error);
    assert_progress_beside(&host, |_| {});
    // The failing run keeps its worker: nothing about it was decided while it failed.
    assert!(host.runtime.running(&failing_worker));
    assert_eq!(host.state(&failing).0, "running");
}

#[test]
fn a_worker_that_cannot_start_does_not_hold_back_other_runs() {
    let host = Host::new();
    assert_progress_beside(&host, |host| {
        let blocked = host.start("blocked");
        host.pass(1);
        let worker = host.worker(&blocked);
        host.runtime.failed_starts.lock().unwrap().insert(worker);
    });
}

#[test]
fn a_worker_that_cannot_stop_does_not_hold_back_other_cleanup() {
    let host = Host::new();
    let (stubborn, stubborn_worker) = running_run(&host, "stubborn");
    host.runtime
        .failed_stops
        .lock()
        .unwrap()
        .insert(stubborn_worker.clone());
    assert_progress_beside(&host, |host| host.cancel(&stubborn));
    assert!(host.runtime.running(&stubborn_worker));
    assert_ne!(host.state(&stubborn).1, "terminal");
}

fn schedule_work_request(host: &Host, schedule: &str, mission_revision: &str) -> String {
    let schedule_revision = host
        .store
        .claims_for(schedule, Some("intent.desired"))
        .unwrap()
        .last()
        .unwrap()
        .id
        .clone();
    host.store
        .append_claim(&ClaimInput {
            subject: schedule.into(),
            kind: "schedule.work-requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("revision".into(), Value::String(schedule_revision)),
                ("occurrence".into(), Value::from(1)),
                ("mission".into(), Value::String("mission/worker".into())),
                (
                    "mission_revision".into(),
                    Value::String(mission_revision.into()),
                ),
                (
                    "workspace".into(),
                    Value::String("/tmp/st3-fault-isolation".into()),
                ),
                ("inputs".into(), Value::Object(Default::default())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .id
}

fn schedule_source(name: &str, revision: &str) -> String {
    format!(
        r#"  schedule "{name}" {{
    at "2099-01-01T00:00:00.000Z"
    work {{
      mission "worker@{revision}"
      workspace "/tmp/st3-fault-isolation"
    }}
  }}"#
    )
}

fn worker_revision(host: &Host) -> String {
    host.store
        .mission_spec("worker", None)
        .unwrap()
        .unwrap()
        .revision
}

fn schedule_named(host: &Host, run: &MissionRunView, name: &str) -> String {
    host.store
        .desired_subjects_for_owner_run(&run.subject)
        .unwrap()
        .into_iter()
        .find(|desired| desired.kind == "schedule" && desired.subject.ends_with(name))
        .map(|desired| desired.subject)
        .unwrap_or_else(|| panic!("run {} declares no schedule {name}", run.id))
}

/// example-peer, mid-sync: scheduled work named a mission revision the host had not received yet.
#[test]
fn scheduled_work_for_a_revision_this_host_lacks_waits_beside_other_schedules() {
    let host = Host::new();
    let revision = worker_revision(&host);
    let intake = host.start_intake(&format!(
        "{}\n{}",
        schedule_source("behind", &revision),
        schedule_source("current", &revision)
    ));
    let behind = schedule_named(&host, &intake, "behind");
    let current = schedule_named(&host, &intake, "current");
    let missing = "f".repeat(64);
    assert_progress_beside(&host, |host| {
        schedule_work_request(host, &behind, &missing);
        schedule_work_request(host, &current, &revision);
    });
    let reason = host
        .fault(&behind, "schedule-work")
        .expect("the schedule records why its work waits");
    assert!(reason.contains("missing-mission"), "{reason}");
    assert_eq!(host.fault(DAEMON, "stage/scheduled-work"), None);
    // The request waits for the revision instead of failing.
    assert_eq!(
        host.store
            .pending_schedule_work_requests(&behind)
            .unwrap()
            .len(),
        1
    );
    assert!(
        host.store
            .claims_for(&behind, Some("schedule.work-failed"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        host.store
            .claims_for(&current, Some("schedule.work-started"))
            .unwrap()
            .len(),
        1,
        "the other schedule's work did not start"
    );
}

/// Every peer replicates a schedule's work requests. Only the host that requested the work
/// starts it, so a peer never starts a duplicate run or fails on a revision it lacks.
#[test]
fn scheduled_work_starts_only_on_the_host_that_requested_it() {
    let host = Host::new();
    let revision = worker_revision(&host);
    let intake = host.start_intake(&schedule_source("nightly", &revision));
    let schedule = schedule_named(&host, &intake, "nightly");
    schedule_work_request(&host, &schedule, &revision);
    let peer = Reconciler::new(
        host.store.clone(),
        host.runtime.clone(),
        "peer".into(),
        Arc::new(Notify::new()),
    );
    peer.reconcile_once().unwrap();
    assert!(
        host.store
            .claims_for(&schedule, Some("schedule.work-started"))
            .unwrap()
            .is_empty(),
        "a peer started work that another host requested"
    );
    host.pass(1);
    assert_eq!(
        host.store
            .claims_for(&schedule, Some("schedule.work-started"))
            .unwrap()
            .len(),
        1
    );
}

/// A request that cannot start for a lasting reason fails, so its schedule can fire again.
#[test]
fn scheduled_work_that_cannot_start_fails_without_freezing_its_schedule() {
    let host = Host::new();
    let revision = worker_revision(&host);
    let intake = host.start_intake(&schedule_source("nightly", &revision));
    let schedule = schedule_named(&host, &intake, "nightly");
    let request = host
        .store
        .append_claim(&ClaimInput {
            subject: schedule.clone(),
            kind: "schedule.work-requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("revision".into(), Value::String("unused".into())),
                ("occurrence".into(), Value::from(1)),
                ("mission".into(), Value::String("mission/worker".into())),
                ("mission_revision".into(), Value::String(revision.clone())),
                (
                    "workspace".into(),
                    Value::String("/tmp/st3-fault-isolation".into()),
                ),
                (
                    "inputs".into(),
                    serde_json::json!({ "undeclared": "value" }),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    host.pass(2);
    let failures = host
        .store
        .claims_for(&schedule, Some("schedule.work-failed"))
        .unwrap();
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert_eq!(failures[0].body["fields"]["request"], request.id);
    assert!(
        host.store
            .pending_schedule_work_requests(&schedule)
            .unwrap()
            .is_empty()
    );
    schedule_work_request(&host, &schedule, &revision);
    host.pass(1);
    assert_eq!(
        host.store
            .claims_for(&schedule, Some("schedule.work-started"))
            .unwrap()
            .len(),
        1
    );
}

fn subscription_request(
    host: &Host,
    subscription: &str,
    mission_revision: &str,
    discovery: &str,
) -> String {
    host.store
        .append_claim(&ClaimInput {
            subject: subscription.into(),
            kind: "subscription.mission-requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("mission".into(), Value::String("mission/review".into())),
                (
                    "mission_revision".into(),
                    Value::String(mission_revision.into()),
                ),
                ("resource".into(), Value::String("resource/repo".into())),
                ("resource_input".into(), Value::String("source".into())),
                (
                    "workspace".into(),
                    Value::String("/tmp/st3-fault-isolation".into()),
                ),
                ("discovery".into(), Value::String(discovery.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .id
}

/// example-linux and ExampleMac, 21:09Z: a subscription delivery pinned to a claim ID instead of a revision.
#[test]
fn a_subscription_delivery_pinned_to_a_claim_id_waits_beside_other_deliveries() {
    let host = Host::new();
    host.publish(
        r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  concurrent-runs max=8
  goal "Review a discovered item."
  step "review" { goal "Review it." }
}"#,
        "publish-review",
    );
    let revision = host
        .store
        .mission_spec("review", None)
        .unwrap()
        .unwrap()
        .revision;
    let intake = host.start_intake(&format!(
        r#"  observer "repo" {{ resource "resource/repo"; provider "github.repository"; locator "example/repo"; field "pull_requests" }}
  subscription "reviews" {{
    observer "observer/repo"
    on "pull_requests"
    delivery "mission" {{
      mission "review@{revision}"
      resource "source"
      workspace "/tmp/st3-fault-isolation"
    }}
  }}"#
    ));
    let subscription = host.owned(&intake, "subscription");
    let requests = std::cell::RefCell::new(Vec::new());
    assert_progress_beside(&host, |host| {
        let discovery = host
            .store
            .append_claim(&ClaimInput {
                subject: "resource/repo".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), Value::String("vcs.repository".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
            .id;
        // A claim ID where the mission revision belongs, then a delivery that is fine.
        let pinned = subscription_request(host, &subscription, &discovery, &discovery);
        let valid = subscription_request(host, &subscription, &revision, &discovery);
        *requests.borrow_mut() = vec![pinned, valid];
    });
    let requests = requests.into_inner();
    let reason = host
        .fault(&subscription, "subscription")
        .expect("the subscription records why its delivery waits");
    assert!(
        reason.contains(&requests[0]) && reason.contains("missing-mission"),
        "{reason}"
    );
    assert_eq!(host.fault(DAEMON, "stage/subscriptions"), None);
    assert!(
        host.store
            .claims_for(&subscription, Some("subscription.mission-failed"))
            .unwrap()
            .is_empty(),
        "a delivery that may still arrive was failed"
    );
    let started = host
        .store
        .claims_for(&subscription, Some("subscription.mission-started"))
        .unwrap();
    assert_eq!(started.len(), 1, "{started:#?}");
    assert_eq!(
        started[0].body["fields"]["request"].as_str(),
        Some(requests[1].as_str())
    );
}

/// One failing schedule, subscription, or observer does not hold back the others of its kind.
#[test]
fn a_failing_intake_item_does_not_hold_back_its_siblings() {
    for fault in [Fault::Error, Fault::Panic] {
        let host = Host::new();
        host.publish(
            r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  concurrent-runs max=8
  goal "Review a discovered item."
  step "review" { goal "Review it." }
}"#,
            "publish-review",
        );
        let review = host
            .store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let worker = worker_revision(&host);
        let subscription = |name: &str| {
            format!(
                r#"  observer "{name}" {{ resource "resource/repo"; provider "github.repository"; locator "example/repo"; field "pull_requests" }}
  subscription "{name}" {{
    observer "observer/{name}"
    on "pull_requests"
    delivery "mission" {{
      mission "review@{review}"
      resource "source"
      workspace "/tmp/st3-fault-isolation"
    }}
  }}"#
            )
        };
        let intake = host.start_intake(&format!(
            "{}\n{}\n{}\n{}",
            schedule_source("first", &worker),
            schedule_source("second", &worker),
            subscription("first"),
            subscription("second"),
        ));
        let owned = host
            .store
            .desired_subjects_for_owner_run(&intake.subject)
            .unwrap();
        let named = |kind: &str, name: &str| {
            owned
                .iter()
                .find(|desired| desired.kind == kind && desired.subject.ends_with(name))
                .map(|desired| desired.subject.clone())
                .unwrap()
        };
        // The first of each kind sorts before its sibling, so it is taken up first.
        for (scope, kind) in [
            ("observer", "observer"),
            ("schedule-work", "schedule"),
            ("subscription", "subscription"),
        ] {
            host.faults.fail(scope, &named(kind, "first"), fault);
        }
        let second_schedule = named("schedule", "second");
        let second_subscription = named("subscription", "second");
        schedule_work_request(&host, &named("schedule", "first"), &worker);
        schedule_work_request(&host, &second_schedule, &worker);
        let discovery = host
            .store
            .append_claim(&ClaimInput {
                subject: "resource/repo".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), Value::String("vcs.repository".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
            .id;
        subscription_request(&host, &named("subscription", "first"), &review, &discovery);
        subscription_request(&host, &second_subscription, &review, &discovery);
        host.pass(2);
        for (scope, kind) in [
            ("observer", "observer"),
            ("schedule-work", "schedule"),
            ("subscription", "subscription"),
        ] {
            assert!(
                host.fault(&named(kind, "first"), scope).is_some(),
                "the failing {kind} recorded no fault"
            );
        }
        assert_eq!(
            host.store
                .claims_for(&second_schedule, Some("schedule.work-started"))
                .unwrap()
                .len(),
            1,
            "the second schedule's work did not start"
        );
        assert_eq!(
            host.store
                .claims_for(&second_subscription, Some("subscription.mission-started"))
                .unwrap()
                .len(),
            1,
            "the second subscription's delivery did not start"
        );
    }
}

fn exec_member(host: &Host, run: &MissionRunView) -> String {
    host.store
        .desired_subjects_for_owner_run(&run.subject)
        .unwrap()
        .into_iter()
        .filter(|desired| desired.kind == "exec")
        .find_map(|desired| desired.member)
        .map(|member| member.runtime_id)
        .unwrap_or_else(|| panic!("run {} declares no exec member", run.id))
}

fn actual_status(host: &Host, subject: &str) -> Option<String> {
    host.store
        .latest_actual_value(subject)
        .unwrap()
        .and_then(|actual| {
            actual
                .get("fields")
                .unwrap_or(&actual)
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
}

/// While the PTY registry does not answer, a terminal seat is neither started, restarted, nor
/// judged stopped, and everything else on the host still runs.
#[test]
fn an_unavailable_pty_snapshot_holds_only_terminal_members() {
    let host = Host::new();
    let (cancelled, cancelled_worker) = running_run(&host, "cancelled");
    let seat = host.owned(&cancelled, "agent");
    host.runtime
        .snapshot_unavailable
        .store(true, Ordering::SeqCst);
    host.cancel(&cancelled);
    let fresh = host.start("fresh");
    let job = host.start_mission("job", "job");
    host.pass(8);
    assert!(
        host.runtime.running(&exec_member(&host, &job)),
        "an exec member did not start while the PTY snapshot was unavailable"
    );
    assert_eq!(
        actual_status(&host, &seat).as_deref(),
        Some("running"),
        "the cancelled seat was judged without a snapshot"
    );
    assert_ne!(host.state(&cancelled).1, "terminal");
    assert!(
        !host.runtime.running(&host.worker(&fresh)),
        "a seat started without knowing whether it already runs"
    );
    host.runtime
        .snapshot_unavailable
        .store(false, Ordering::SeqCst);
    host.pass(8);
    assert!(!host.runtime.running(&cancelled_worker));
    assert_eq!(
        host.state(&cancelled),
        ("cancelled".into(), "terminal".into())
    );
    assert!(host.runtime.running(&host.worker(&fresh)));
}

/// A PTY whose record cannot be read may still run, so its stop waits instead of recording it
/// stopped.
#[test]
fn a_pty_in_an_unknown_state_is_never_recorded_stopped() {
    let host = Host::new();
    let (cancelled, cancelled_worker) = running_run(&host, "cancelled");
    let seat = host.owned(&cancelled, "agent");
    host.runtime
        .execs
        .lock()
        .unwrap()
        .get_mut(&cancelled_worker)
        .unwrap()
        .status = "unknown".into();
    host.cancel(&cancelled);
    host.pass(8);
    assert_ne!(actual_status(&host, &seat).as_deref(), Some("stopped"));
    assert_ne!(host.state(&cancelled).1, "terminal");
    host.runtime
        .execs
        .lock()
        .unwrap()
        .get_mut(&cancelled_worker)
        .unwrap()
        .status = "running".into();
    host.pass(8);
    assert_eq!(
        host.state(&cancelled),
        ("cancelled".into(), "terminal".into())
    );
}

/// One step of a run that fails does not hold back the run's other steps.
#[test]
fn a_failing_step_does_not_hold_back_the_other_steps_of_its_run() {
    for fault in [Fault::Error, Fault::Panic] {
        let host = Host::new();
        let run = host.start_mission("pair", "pair");
        let step = |path: &str| {
            host.store
                .mission_run(&run.id)
                .unwrap()
                .unwrap()
                .steps
                .into_iter()
                .find(|step| step.step == path)
                .unwrap()
        };
        // The failing step comes first in the run's step order.
        let first = step("first").subject;
        host.faults.fail("step", &first, fault);
        host.pass(4);
        assert_eq!(step("first").status, "pending");
        assert_eq!(step("second").status, "ready");
        assert!(host.fault(&first, "step").is_some());
        assert_eq!(host.fault(&run.subject, "mission-run"), None);
        host.faults.clear();
        host.pass(2);
        assert_eq!(step("first").status, "ready");
        assert_eq!(host.fault(&first, "step"), None);
    }
}

/// A runtime that never stops ends its run at the cleanup deadline instead of holding it and its
/// active-run slot forever. Its stop stays declared and completes once the runtime can stop.
#[test]
fn a_runtime_that_never_stops_ends_its_run_at_the_cleanup_deadline() {
    let host = Host::with_cleanup_deadline(Some(std::time::Duration::ZERO));
    let (stubborn, stubborn_worker) = running_run(&host, "stubborn");
    host.runtime
        .failed_stops
        .lock()
        .unwrap()
        .insert(stubborn_worker.clone());
    host.cancel(&stubborn);
    host.pass(8);
    let ended = host.store.mission_run(&stubborn.id).unwrap().unwrap();
    assert_eq!(
        (ended.status.as_str(), ended.phase.as_str()),
        ("cancelled", "terminal")
    );
    let reason = host
        .store
        .latest_claim(&stubborn.subject, Some("mission-run.state"))
        .unwrap()
        .unwrap()
        .body["fields"]["reason"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(reason.contains("still live"), "{reason}");
    assert!(host.runtime.running(&stubborn_worker));
    host.runtime.failed_stops.lock().unwrap().clear();
    // The failed stop waits out the worker's shutdown timeout and then kills it.
    std::thread::sleep(std::time::Duration::from_millis(60));
    host.pass(4);
    assert!(
        !host.runtime.running(&stubborn_worker),
        "the stop was abandoned when its run ended"
    );
}

/// Replicate `source`, which runs as `label`, into `target` until both hold the same claims, and
/// project what arrived.
fn replicate(source: &Store, label: &str, target: &Store) {
    const FLEET: &str = "fault-isolation";
    source.bind_fleet(FLEET).unwrap();
    target.bind_fleet(FLEET).unwrap();
    for _ in 0..100 {
        let inventory = target.replication_inventory().unwrap();
        if inventory.digest == source.replication_inventory().unwrap().digest {
            break;
        }
        let exchange = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange(label, FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
    }
    assert!(
        target.project_replication_backlog().unwrap(),
        "the graph is stale: {:?}",
        quarantined(target)
    );
}

/// The quarantined claims a store names, as `(aggregate, message)`.
fn quarantined(store: &Store) -> Vec<(String, String)> {
    store
        .replication_status(false, None, &[])
        .unwrap()
        .unhealthy
        .into_iter()
        .map(|projection| {
            (
                projection.aggregate,
                projection.error_message.unwrap_or_default(),
            )
        })
        .collect()
}

/// A planning claim that admission accepts but the planning projection cannot read, such as one
/// from a faulty or older producer, is quarantined on its own. The peer that receives it still
/// projects every other claim and starts their work, and the store that holds it still opens.
#[test]
fn a_planning_claim_that_cannot_be_projected_holds_back_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.sqlite3");
    let source = Store::open(&path, "source").unwrap();
    let bad = source
        .append_claim(&ClaimInput {
            subject: "planning-session/bad".into(),
            kind: "planning-session.started".into(),
            actor: Some("person/operator".into()),
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let healthy = r#"version 2
mission "healthy" state="ready" {
  goal "Run beside a planning claim that cannot be projected."
  agent "worker" { workspace "${ST_WORKSPACE}"; command "sleep 600"; restart "never" }
  step "wait" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Wait." }
}"#;
    let intent = st3::parse_intent(healthy, "source").unwrap();
    let preview = source
        .mission(
            &intent,
            IntentInput {
                kdl: healthy.into(),
                source_name: None,
            },
        )
        .unwrap();
    source
        .apply(&intent, &preview.subject_tokens, "publish-healthy")
        .unwrap();
    let expected = |store: &Store| {
        let quarantined = quarantined(store);
        assert_eq!(quarantined.len(), 1, "{quarantined:?}");
        assert_eq!(quarantined[0].0, format!("projection:planning:{}", bad.id));
        assert!(
            quarantined[0].1.contains("planning-session/bad"),
            "{quarantined:?}"
        );
    };

    let host = Host::new();
    replicate(&source, "source", &host.store);
    let run = host.start_mission("healthy", "healthy");
    host.pass(4);
    assert!(host.runtime.running(&host.worker(&run)));
    expected(&host.store);

    drop(source);
    let source = Store::open(&path, "source").unwrap();
    expected(&source);
    source.rebuild_claim_projections().unwrap();
    expected(&source);
}

/// Two missions in one document start separate runs, so an agent one of them declares for its run
/// never resolves the other's reference. Publication refuses the mission that declares nothing,
/// and when such a mission was published before that check, its run waits while the other runs.
#[test]
fn another_missions_agent_never_resolves_a_missing_one() {
    let host = Host::new();
    let source = r#"version 2
mission "unstaffed" state="ready" {
  goal "Select a helper this mission never declares."
  step "work" { assigned-to "agent/${ST_MISSION_RUN}/fleet.helper"; goal "Do the work." }
}
mission "staffed" state="ready" {
  goal "Select the helper this mission declares."
  agent "helper" { identity "fleet.helper"; workspace "${ST_WORKSPACE}"; command "sleep 600"; restart "never" }
  step "work" { assigned-to "agent/${ST_MISSION_RUN}/fleet.helper"; goal "Do the work." }
}"#;
    let missing = "mission `mission/unstaffed` references missing eligible agent `agent/${ST_MISSION_RUN}/fleet.helper`";
    assert_eq!(
        host.store
            .unresolved_references(&st3::parse_intent(source, HOST).unwrap())
            .unwrap(),
        [missing]
    );

    host.publish(source, "publish-before-the-check");
    let unstaffed = host.start_mission("unstaffed", "unstaffed");
    let staffed = host.start_mission("staffed", "staffed");
    host.pass(2);
    assert!(host.runtime.running(&host.worker(&staffed)));
    assert!(
        host.store
            .desired_subjects_for_owner_run(&unstaffed.subject)
            .unwrap()
            .is_empty()
    );
    let step = host
        .store
        .mission_run(&unstaffed.id)
        .unwrap()
        .unwrap()
        .steps[0]
        .clone();
    assert_eq!(step.claimant, None, "{step:?}");
    assert_eq!(host.store.unresolved_graph_references().unwrap(), [missing]);
}
