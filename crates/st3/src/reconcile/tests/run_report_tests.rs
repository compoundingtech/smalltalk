//! A run that names a reporter tells it about a failed, cancelled or stalled run, and a completed
//! one if asked, once for each event. A run that names no reporter says nothing to anyone.
use super::*;

/// Where the frozen test clock starts: now, because step rows are stamped by the real clock and
/// a frozen clock far from it would make a step's stamp meaningless against the run's.
fn start() -> u128 {
    static START: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    *START.get_or_init(now_ms)
}
const MINUTE: u128 = 60_000;
const REPORTER: &str = "agent/node.reporter";

const SOURCE: &str = r#"
version 2

agent "reporter" { workspace "/tmp"; command "true" }
agent "builder" { workspace "/tmp"; command "true" }
resource "loop-result" { kind "custom.test.loop-result" }

mission "watched" state="ready" report-to="agent/node.reporter" stalled-after="10m" {
  concurrent-runs
  goal "Run work that reports to an agent."
  step "build" { assigned-to "agent/node.builder" }
  step "ship" {
    assigned-to "agent/node.builder"
    depends-on { step "build" completed }
  }
}

mission "watched-with-default-limit" state="ready" report-to="agent/node.reporter" {
  concurrent-runs
  goal "Run work that reports to an agent with the default stall limit."
  step "build" { assigned-to "agent/node.builder" }
}

mission "celebrated" state="ready" report-to="agent/node.reporter" report-completed=#true {
  concurrent-runs
  goal "Report completion too."
  step "build" { assigned-to "agent/node.builder" }
}

mission "owned-by-reporter" state="ready" report-to="agent/node.reporter" {
  concurrent-runs
  goal "Report to the agent that owns a step."
  step "build" { assigned-to "agent/node.reporter" }
}

mission "unwatched" state="ready" {
  concurrent-runs
  goal "Name no reporter."
  step "build" { assigned-to "agent/node.builder" }
}

mission "absent-reporter" state="ready" report-to="agent/node.nobody" {
  concurrent-runs
  goal "Name an agent that is not declared."
  step "build" { assigned-to "agent/node.builder" }
}
mission "loop-parent" state="ready" report-to="agent/node.reporter" stalled-after="10m" {
  concurrent-runs
  goal "Watch progress in child rounds."
  loop "refine" {
    max-rounds 3
    until { gate "done" { field "state" "resource/loop-result" is "done" } }
    round {
      completion { when "all-steps-exhausted" }
      step "work" { assigned-to "agent/node.builder" }
    }
  }
}
"#;

struct Clock;
impl Clock {
    fn at(store: &Store, at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        store.set_write_clock_at(at).unwrap();
        Self
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

struct Fixture {
    store: Arc<Store>,
    reconciler: Reconciler<FakeRuntime>,
}

fn fixture() -> (Clock, Fixture) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let clock = Clock::at(&store, start());
    apply_source(&store, SOURCE, "run-report-missions");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    reconciler.reconcile_once().unwrap();
    (clock, Fixture { store, reconciler })
}

impl Fixture {
    fn start(&self, mission: &str, key: &str) -> crate::model::MissionRunView {
        let run = self
            .store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap();
        self.reconciler.reconcile_once().unwrap();
        run
    }

    fn passes(&self, count: usize) {
        for _ in 0..count {
            self.reconciler.reconcile_once().unwrap();
        }
    }

    fn reports(&self, to: &str) -> Vec<crate::model::MessageView> {
        self.store
            .messages(Some(to), true)
            .unwrap()
            .into_iter()
            .filter(|message| message.tags.iter().any(|tag| tag.starts_with("st3-run-report:")))
            .collect()
    }

    fn report_claims(&self) -> usize {
        self.store
            .messages(None, true)
            .unwrap()
            .into_iter()
            .filter(|message| message.tags.iter().any(|tag| tag.starts_with("st3-run-report:")))
            .count()
    }

    fn step(run: &crate::model::MissionRunView, path: &str) -> String {
        run.steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .subject
            .clone()
    }

    fn faults(&self, run: &crate::model::MissionRunView) -> Vec<String> {
        self.store
            .claims_for(&run.subject, Some("reconcile.fault"))
            .unwrap()
            .into_iter()
            .filter(|claim| claim.body["fields"]["scope"] == "report-to")
            .map(|claim| claim.body["fields"]["status"].as_str().unwrap().to_owned())
            .collect()
    }
}

#[test]
fn a_quiet_parent_counts_child_round_progress_and_then_reports_real_silence() {
    for inline in [false, true] {
        let (_clock, fixture) = fixture();
        if inline {
            let source = SOURCE.replace(
                r#"  loop "refine" {"#,
                "  step \"outer\" {\n    agentless\n    mission \"nested\" {\n      goal \"Work in an inline loop.\"\n      loop \"refine\" {",
            ) + "\n}\n}\n";
            apply_source(&fixture.store, &source, "inline-loop-report");
        }
        let parent = fixture.start("loop-parent", "loop-parent-activity");
        fixture.passes(10);
        let loop_step = parent
            .steps
            .iter()
            .find(|step| step.step.ends_with("refine"))
            .unwrap();
        let child = fixture
            .store
            .mission_run_for_parent_step(&loop_step.subject)
            .unwrap()
            .unwrap();
        let builder = fixture
            .reconciler
            .runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|member| member.runtime_id.ends_with("builder"))
            .unwrap()
            .clone();
        fixture
            .reconciler
            .runtime
            .ptys
            .lock()
            .unwrap()
            .push(RuntimeObservation {
                runtime_id: builder.runtime_id,
                terminal: builder.terminal,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("round-worker".into()),
            });
        fixture.passes(1);
        let request = |key: &str, summary: Option<&str>| crate::model::WorkRequest {
            actor: Some("agent/node.builder".into()),
            incarnation: Some("round-worker".into()),
            summary: summary.map(str::to_owned),
            reason: None,
            evidence: Vec::new(),
            wait_ms: None,
            wait_on: None,
            idempotency_key: key.into(),
        };
        fixture
            .store
            .work_action(
                &child.steps[0].subject,
                "claim",
                &request("claim-round", None),
            )
            .unwrap();
        let parent_view = fixture.store.mission_run(&parent.id).unwrap().unwrap();
        let quiet_since = crate::reconcile::run_report::run_activity(&parent_view);
        let progress_at = quiet_since + 9 * MINUTE;
        let _progress = Clock::at(&fixture.store, progress_at);
        fixture
            .store
            .work_action(
                &child.steps[0].subject,
                "progress",
                &request("child-round-progress", Some("Round is making progress.")),
            )
            .unwrap();
        fixture.passes(2);
        // Only the child changed; its progress invalidates the parent's evaluation and deadline.
        let held = fixture
            .store
            .step_run(&child.steps[0].subject)
            .unwrap()
            .unwrap();
        assert_eq!(held.status, "working");
        assert_eq!(held.progress_at_unix_ms, Some(progress_at));
        assert_eq!(
            held.claim_expires_at_unix_ms,
            Some(progress_at + 10 * MINUTE)
        );
        let _past_parent_limit = Clock::at(&fixture.store, quiet_since + 11 * MINUTE);
        fixture.passes(2);
        assert_eq!(
            crate::reconcile::run_report::run_activity(
                &fixture.store.mission_run(&parent.id).unwrap().unwrap()
            ),
            quiet_since,
            "Only the child round made progress"
        );
        let parent_reports = || {
            fixture
                .reports(REPORTER)
                .into_iter()
                .filter(|message| {
                    message
                        .tags
                        .contains(&format!("mission-run:{}", parent.subject))
                })
                .collect::<Vec<_>>()
        };
        assert!(
            parent_reports().is_empty(),
            "Child round is active: {:?}",
            parent_reports()
        );
        assert_eq!(
            fixture.reconciler.incremental.next_due(&parent.subject),
            Some(progress_at + 20 * MINUTE)
        );
        // The live worker's progress renews its lease for ten minutes. Release it before
        // checking true silence, so a later lease-expiry transition cannot count as activity.
        fixture
            .store
            .work_action(
                &child.steps[0].subject,
                "release",
                &request("release-round", None),
            )
            .unwrap();
        fixture.passes(2);
        let _real_silence = Clock::at(&fixture.store, quiet_since + 22 * MINUTE);
        fixture.passes(2);
        assert_eq!(parent_reports().len(), 1);
        fixture.passes(3);
        assert_eq!(parent_reports().len(), 1);
    }
}

#[test]
fn a_failed_run_is_reported_once_without_the_failure_text() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("watched", "failed-run");
    fixture
        .store
        .set_step_state(
            &Fixture::step(&run, "build"),
            "failed",
            Some("deploy token hunter2 was rejected"),
        )
        .unwrap();
    fixture.passes(6);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let message = &reports[0];
    assert_eq!(message.from, "daemon/runtime");
    assert!(message.tags.contains(&"st3-run-report:failed".to_owned()));
    assert!(message.tags.contains(&format!("mission-run:{}", run.subject)));
    assert!(message.content.contains(&run.subject), "{}", message.content);
    assert!(message.content.contains("build (failed)"), "{}", message.content);
    assert!(!message.content.contains("hunter2"), "{}", message.content);
    assert!(!message.title.clone().unwrap().contains("hunter2"));
    // Later passes, a restart's first full pass and further activity say nothing more.
    let restarted = Reconciler::new(
        fixture.store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    restarted.reconcile_once().unwrap();
    restarted.reconcile_once().unwrap();
    assert_eq!(fixture.reports(REPORTER).len(), 1);
}

#[test]
fn a_cancelled_run_is_reported_without_the_reason() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("watched", "cancelled-run");
    fixture
        .store
        .request_mission_run_cancellation(&run.id, "api key sk-live-123 leaked into the log")
        .unwrap();
    fixture.passes(6);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].tags.contains(&"st3-run-report:cancelled".to_owned()));
    assert!(!reports[0].content.contains("sk-live-123"));
    assert_eq!(
        fixture.store.mission_run(&run.id).unwrap().unwrap().status,
        "cancelled"
    );
}

#[test]
fn a_queued_run_gets_its_full_timeout_after_admission() {
    let (_clock, fixture) = fixture();
    let source = SOURCE.replace(
        r#"mission "watched-with-default-limit" state="ready""#,
        r#"mission "watched-with-default-limit" state="ready" timeout="5m""#,
    );
    apply_source(&fixture.store, &source, "queued-timeout-missions");
    let predecessor = fixture.start("unwatched", "timeout-predecessor");
    let revision = fixture
        .store
        .mission_spec("watched-with-default-limit", None)
        .unwrap()
        .unwrap()
        .revision;
    apply_source(
        &fixture.store,
        &format!(
            "version 2\nmission-run \"timed-successor\" {{\n  mission \"mission/watched-with-default-limit@{revision}\"\n  workspace \"/tmp\"\n  requester \"person/operator\"\n  after {:?}\n}}\n",
            predecessor.subject,
        ),
        "queued-timeout-run",
    );
    let queued = fixture
        .store
        .mission_run("timed-successor")
        .unwrap()
        .unwrap();
    assert_eq!(queued.timeout_ms, Some(5 * MINUTE as u64));
    assert_eq!(
        queued.deadline_at_unix_ms, None,
        "Waiting is outside the run timeout"
    );
    let replica = Store::open_memory("replica").unwrap();
    replica
        .import_replication("node", &fixture.store.export_replication(0).unwrap())
        .unwrap();
    let replicated = replica.mission_run("timed-successor").unwrap().unwrap();
    assert_eq!(replicated.timeout_ms, queued.timeout_ms);
    assert_eq!(replicated.deadline_at_unix_ms, None);

    let admitted_at = start() + 6 * MINUTE;
    let _past_creation_budget = Clock::at(&fixture.store, admitted_at);
    fixture.passes(3);
    let waiting = fixture
        .store
        .mission_run("timed-successor")
        .unwrap()
        .unwrap();
    assert_eq!(waiting.phase, "normal");
    assert_eq!(
        waiting
            .steps
            .iter()
            .find(|step| step.step == "after-run")
            .unwrap()
            .status,
        "working"
    );
    assert_eq!(waiting.deadline_at_unix_ms, None);
    assert_eq!(
        fixture.store.next_active_mission_deadline("node").unwrap(),
        None
    );

    fixture
        .store
        .set_mission_run_state(&predecessor.id, "completed", "terminal", None)
        .unwrap();
    fixture.passes(3);
    let admitted = fixture
        .store
        .mission_run("timed-successor")
        .unwrap()
        .unwrap();
    let deadline = admitted_at + 5 * MINUTE;
    assert_eq!(admitted.deadline_at_unix_ms, Some(deadline));
    assert_eq!(admitted.timeout_ms, Some(5 * MINUTE as u64));
    assert_eq!(
        admitted
            .steps
            .iter()
            .find(|step| step.step == "after-run")
            .unwrap()
            .status,
        "completed"
    );
    assert_eq!(
        fixture.store.next_active_mission_deadline("node").unwrap(),
        Some(deadline)
    );
    replica
        .import_replication("node", &fixture.store.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        replica
            .mission_run("timed-successor")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        Some(deadline)
    );

    let _before_deadline = Clock::at(&fixture.store, deadline - 1);
    fixture.passes(3);
    assert_eq!(
        fixture
            .store
            .mission_run("timed-successor")
            .unwrap()
            .unwrap()
            .phase,
        "normal"
    );
    let _at_deadline = Clock::at(&fixture.store, deadline);
    fixture.passes(6);
    assert_eq!(
        fixture
            .store
            .mission_run("timed-successor")
            .unwrap()
            .unwrap()
            .status,
        "failed"
    );
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0]
            .tags
            .contains(&"st3-run-report:failed".to_owned())
    );
    // A late join uses the accepted admission stamp, even after expiry.
    let late = Store::open_memory("late-replica").unwrap();
    late.import_replication("node", &fixture.store.export_replication(0).unwrap())
        .unwrap();
    assert_eq!(
        late.mission_run("timed-successor")
            .unwrap()
            .unwrap()
            .deadline_at_unix_ms,
        Some(deadline)
    );
}

#[test]
fn a_timed_out_run_is_reported_as_failed() {
    let source = SOURCE.replace(
        r#"mission "watched-with-default-limit" state="ready""#,
        r#"mission "watched-with-default-limit" state="ready" timeout="5m""#,
    );
    let store = Arc::new(Store::open_memory("node").unwrap());
    let _clock = Clock::at(&store, start());
    apply_source(&store, &source, "run-report-timeout");
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.reconcile_once().unwrap();
    let fixture = Fixture { store, reconciler };
    fixture.start("watched-with-default-limit", "timed-out");
    let _later = Clock::at(&fixture.store, start() + 6 * MINUTE);
    fixture.passes(4);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].tags.contains(&"st3-run-report:failed".to_owned()));
}

#[test]
fn a_stalled_run_is_reported_once_and_again_only_after_more_progress() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("watched", "stalled-run");
    let build = Fixture::step(&run, "build");
    fixture.passes(3);
    assert!(fixture.reports(REPORTER).is_empty());
    let started = fixture.store.mission_run(&run.id).unwrap().unwrap();
    let quiet_since = crate::reconcile::run_report::run_activity(&started);

    // Nine minutes in, the 10-minute limit has not passed.
    let nine = Clock::at(&fixture.store, quiet_since + 9 * MINUTE);
    fixture.passes(3);
    assert!(fixture.reports(REPORTER).is_empty());

    // With no claim written since, only the clock moved: the deadline woke the run.
    let eleven = Clock::at(&fixture.store, quiet_since + 11 * MINUTE);
    fixture.passes(1);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].tags.contains(&"st3-run-report:stalled".to_owned()));
    assert!(reports[0].content.contains("build ("), "{}", reports[0].content);
    assert!(reports[0].content.contains("not failed"), "{}", reports[0].content);
    fixture.passes(8);
    assert_eq!(fixture.reports(REPORTER).len(), 1);

    // Progress makes a new silence a new event. Step rows are stamped by the real clock, so let
    // the frozen one go before the step moves.
    drop((nine, eleven));
    fixture
        .store
        .set_step_state(&build, "completed", None)
        .unwrap();
    fixture.passes(2);
    let moved = fixture.store.mission_run(&run.id).unwrap().unwrap();
    let active_at = crate::reconcile::run_report::run_activity(&moved);
    let _again = Clock::at(&fixture.store, active_at + 11 * MINUTE);
    fixture.passes(2);
    assert_eq!(
        fixture
            .reports(REPORTER)
            .iter()
            .filter(|message| message.tags.contains(&"st3-run-report:stalled".to_owned()))
            .count(),
        2
    );
}

#[test]
fn a_stall_is_a_deadline_on_the_run_not_a_scan() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("watched-with-default-limit", "deadline-run");
    fixture.passes(2);
    let started = fixture.store.mission_run(&run.id).unwrap().unwrap();
    let quiet_since = crate::reconcile::run_report::run_activity(&started);
    // The run's own item is due when the default 30 minutes end, and not before.
    assert_eq!(
        fixture.reconciler.incremental.next_due(&run.subject),
        Some(quiet_since + 30 * MINUTE)
    );
    // A pass that finds nothing changed and nothing due leaves the run alone.
    let _before = Clock::at(&fixture.store, quiet_since + 30 * MINUTE - 1);
    assert!(!fixture.reconciler.incremental.needs(&run.subject, now_ms()));
    let _due = Clock::at(&fixture.store, quiet_since + 30 * MINUTE);
    assert!(fixture.reconciler.incremental.needs(&run.subject, now_ms()));
}

#[test]
fn a_completed_run_is_reported_only_when_asked() {
    let (_clock, fixture) = fixture();
    let asked = fixture.start("celebrated", "completed-asked");
    let unasked = fixture.start("watched-with-default-limit", "completed-unasked");
    for run in [&asked, &unasked] {
        fixture
            .store
            .set_step_state(&Fixture::step(run, "build"), "completed", None)
            .unwrap();
    }
    fixture.passes(6);
    for run in [&asked, &unasked] {
        assert_eq!(
            fixture.store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
    }
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].tags.contains(&"st3-run-report:completed".to_owned()));
    assert!(reports[0].tags.contains(&format!("mission-run:{}", asked.subject)));
}

#[test]
fn a_run_with_no_reporter_says_nothing() {
    let (_clock, fixture) = fixture();
    let failed = fixture.start("unwatched", "unwatched-failed");
    let cancelled = fixture.start("unwatched", "unwatched-cancelled");
    let stalled = fixture.start("unwatched", "unwatched-stalled");
    let completed = fixture.start("unwatched", "unwatched-completed");
    fixture
        .store
        .set_step_state(&Fixture::step(&failed, "build"), "failed", Some("broken"))
        .unwrap();
    fixture
        .store
        .request_mission_run_cancellation(&cancelled.id, "not wanted")
        .unwrap();
    fixture
        .store
        .set_step_state(&Fixture::step(&completed, "build"), "completed", None)
        .unwrap();
    let _late = Clock::at(&fixture.store, start() + 24 * 60 * MINUTE);
    fixture.passes(8);
    let _ = &stalled;
    assert_eq!(fixture.report_claims(), 0);
    assert!(fixture.reports(REPORTER).is_empty());
    assert!(fixture.faults(&failed).is_empty());
    // The creation claim of a run with no reporter carries no report fields at all.
    let created = fixture
        .store
        .latest_claim(&failed.subject, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    for field in ["report_to", "stalled_after_ms", "report_completed"] {
        assert!(created.body["fields"].get(field).is_none(), "{field}");
    }
}

#[test]
fn a_reporter_that_owns_a_step_in_the_run_is_still_told() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("owned-by-reporter", "reporter-owns-step");
    fixture
        .store
        .set_step_state(&Fixture::step(&run, "build"), "failed", Some("broken"))
        .unwrap();
    fixture.passes(6);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].content.contains("build (failed)"));
}

#[test]
fn a_reporter_that_is_not_running_gets_a_fault_on_the_run_and_no_loop() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("absent-reporter", "absent-reporter");
    fixture
        .store
        .set_step_state(&Fixture::step(&run, "build"), "failed", Some("broken"))
        .unwrap();
    fixture.passes(4);
    assert!(fixture.reports("agent/node.nobody").is_empty());
    assert_eq!(fixture.report_claims(), 0);
    // The run ended with its reporter unreachable: one fault, closed with the run.
    assert_eq!(fixture.faults(&run), ["faulted", "recovered"]);
    let claims = fixture.store.index().unwrap();
    fixture.passes(10);
    // Looking again writes nothing: the same fault, no message, no growth.
    assert_eq!(fixture.faults(&run), ["faulted", "recovered"]);
    assert_eq!(fixture.report_claims(), 0);
    // The fake runtime's agents flap on their own; the run, its messages and faults do not.
    let later = fixture.store.changes_since(claims, i64::MAX).unwrap().changes;
    assert!(
        later.iter().all(|change| {
            change.kind == "runtime.observed"
                || !(change.subject == run.subject
                    || change.subject.starts_with("message/")
                    || change.kind == "reconcile.fault")
        }),
        "{later:?}"
    );
    assert!(
        fixture
            .store
            .claims_for(&run.subject, Some("reconcile.fault"))
            .unwrap()
            .iter()
            .all(|claim| {
                claim.body["fields"]["reason"]
                    .as_str()
                    .is_some_and(|reason| !reason.contains("broken"))
            })
    );
}

impl Fixture {
    fn start_by(&self, mission: &str, key: &str, requester: &str) -> crate::model::MissionRunView {
        let run = self
            .store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: mission.into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some(requester.into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap();
        self.reconciler.reconcile_once().unwrap();
        run
    }

    fn report_to(
        &self,
        run: &crate::model::MissionRunView,
        actor: &str,
        to: Option<&str>,
        stalled_after_ms: Option<u64>,
        key: &str,
    ) -> crate::model::MissionRunReportView {
        self.store
            .set_mission_run_report(&run.subject, actor, to, stalled_after_ms, None, false, key)
            .unwrap()
    }
}

#[test]
fn a_quiet_run_that_opts_in_is_reported_stalled_once() {
    let (_clock, fixture) = fixture();
    let run = fixture.start_by("unwatched", "opt-in-quiet", "agent/node.builder");
    fixture.passes(3);
    let started = fixture.store.mission_run(&run.id).unwrap().unwrap();
    let quiet_since = crate::reconcile::run_report::run_activity(&started);

    // Two hours of silence with nobody to tell writes nothing.
    let _late = Clock::at(&fixture.store, quiet_since + 120 * MINUTE);
    fixture.passes(3);
    assert_eq!(fixture.report_claims(), 0);

    // Its requester opts it in with a one-hour limit: the silence is already past it, so the
    // next evaluation tells the watcher once, and no later pass tells it again.
    fixture.report_to(
        &run,
        "agent/node.builder",
        Some(REPORTER),
        Some(60 * MINUTE as u64),
        "opt-in-quiet-report",
    );
    fixture.passes(1);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0]
            .tags
            .contains(&"st3-run-report:stalled".to_owned())
    );
    assert!(
        reports[0].content.contains("its limit is 1h"),
        "{}",
        reports[0].content
    );
    fixture.passes(8);
    assert_eq!(fixture.reports(REPORTER).len(), 1);
    // The run kept the creation claim it started with.
    let created = fixture
        .store
        .latest_claim(&run.subject, Some("mission-run.created"))
        .unwrap()
        .unwrap();
    assert!(created.body["fields"].get("report_to").is_none());
}

#[test]
fn a_changed_reporter_is_told_instead_and_a_cleared_run_tells_nobody() {
    let (_clock, fixture) = fixture();
    let moved = fixture.start("watched", "moved-reporter");
    let cleared = fixture.start("watched", "cleared-reporter");
    fixture.report_to(
        &moved,
        "person/requester",
        Some("agent/node.builder"),
        None,
        "moved-report",
    );
    fixture.report_to(&cleared, "person/requester", None, None, "cleared-report");
    for run in [&moved, &cleared] {
        fixture
            .store
            .set_step_state(&Fixture::step(run, "build"), "failed", Some("broken"))
            .unwrap();
    }
    fixture.passes(6);
    assert!(fixture.reports(REPORTER).is_empty());
    let reports = fixture.reports("agent/node.builder");
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0]
            .tags
            .contains(&format!("mission-run:{}", moved.subject))
    );
    assert!(
        reports[0]
            .tags
            .contains(&"st3-run-report:failed".to_owned())
    );
    assert_eq!(fixture.report_claims(), 1);

    // A finished run takes no reporter.
    let finished = fixture
        .store
        .set_mission_run_report(
            &moved.subject,
            "person/requester",
            Some(REPORTER),
            None, None,
            false,
            "finished-report",
        )
        .unwrap_err();
    assert_eq!(finished.code, "mission-run-not-running");
}

#[test]
fn clearing_an_unreachable_reporter_closes_its_fault() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("absent-reporter", "absent-then-cleared");
    fixture.passes(2);
    let started = fixture.store.mission_run(&run.id).unwrap().unwrap();
    let quiet_since = crate::reconcile::run_report::run_activity(&started);
    let _late = Clock::at(&fixture.store, quiet_since + 31 * MINUTE);
    fixture.passes(2);
    assert_eq!(fixture.faults(&run), ["faulted"]);
    fixture.report_to(&run, "person/requester", None, None, "absent-cleared");
    fixture.passes(2);
    assert_eq!(fixture.faults(&run), ["faulted", "recovered"]);
    fixture.passes(4);
    assert_eq!(fixture.faults(&run), ["faulted", "recovered"]);
    assert_eq!(fixture.report_claims(), 0);
}

#[test]
fn opting_out_of_stalls_removes_the_deadline_and_keeps_terminal_reports() {
    let (_clock, fixture) = fixture();
    for (outcome, event) in [
        ("failed", "failed"),
        ("cancelled", "cancelled"),
        ("completed", "completed"),
    ] {
        let run = fixture.start("watched", &format!("muted-{outcome}"));
        fixture.passes(2);
        assert!(
            fixture
                .reconciler
                .incremental
                .next_due(&run.subject)
                .is_some()
        );
        let report = fixture
            .store
            .set_mission_run_report(
                &run.subject,
                "person/requester",
                Some(REPORTER),
                None,
                Some(false),
                true,
                &format!("mute-{outcome}"),
            )
            .unwrap();
        assert!(!report.report_stalls);
        fixture.passes(2);
        assert_eq!(fixture.reconciler.incremental.next_due(&run.subject), None);
        let _late = Clock::at(&fixture.store, start() + 120 * MINUTE);
        fixture.passes(2);
        assert!(fixture.reports(REPORTER).iter().all(|message| {
            !message
                .tags
                .contains(&format!("mission-run:{}", run.subject))
        }));
        if outcome == "failed" {
            fixture
                .store
                .set_step_state(&Fixture::step(&run, "build"), "failed", None)
                .unwrap();
        } else if outcome == "completed" {
            for step in ["build", "ship"] {
                fixture
                    .store
                    .set_step_state(&Fixture::step(&run, step), "completed", None)
                    .unwrap();
            }
        } else {
            fixture
                .store
                .request_mission_run_cancellation(&run.id, "cancel fixture")
                .unwrap();
        }
        fixture.passes(6);
        let reports = fixture.reports(REPORTER);
        let reports: Vec<_> = reports
            .iter()
            .filter(|message| {
                message
                    .tags
                    .contains(&format!("mission-run:{}", run.subject))
            })
            .collect();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].tags.contains(&format!("st3-run-report:{event}")));
    }
}

#[test]
fn stall_reporting_can_be_reenabled_without_fabricating_progress() {
    let (_clock, fixture) = fixture();
    let run = fixture.start("watched", "toggle-stalls");
    fixture
        .store
        .set_mission_run_report(
            &run.subject,
            "person/requester",
            Some(REPORTER),
            None,
            Some(false),
            false,
            "disable-stalls",
        )
        .unwrap();
    fixture.passes(2);
    let _late = Clock::at(&fixture.store, start() + 120 * MINUTE);
    fixture.passes(2);
    assert_eq!(fixture.report_claims(), 0);
    fixture
        .store
        .set_mission_run_report(
            &run.subject,
            "person/requester",
            Some(REPORTER),
            None,
            Some(true),
            false,
            "enable-stalls",
        )
        .unwrap();
    fixture.passes(2);
    assert_eq!(fixture.reports(REPORTER).len(), 1);
    assert!(
        fixture.reports(REPORTER)[0]
            .tags
            .contains(&"st3-run-report:stalled".into())
    );
}

/// The schedule's actual timer runs in an isolated, paused Tokio runtime. The claim clock
/// advances separately so thirty minutes of parent silence need no wall-clock wait.
#[tokio::test(start_paused = true)]
async fn a_muted_retirement_parent_keeps_its_schedule_and_child_reports() {
    use chrono::{DateTime, SecondsFormat, Utc};
    let (_clock, mut fixture) = fixture();
    let root = tempfile::tempdir().unwrap();
    let child_revision = fixture
        .store
        .mission_spec("watched-with-default-limit", None)
        .unwrap()
        .unwrap()
        .revision;
    let anchor_ms = start() + 60 * MINUTE;
    let anchor = DateTime::<Utc>::from_timestamp_millis(anchor_ms as i64)
        .unwrap()
        .to_rfc3339_opts(SecondsFormat::Millis, true);
    apply_source(
        &fixture.store,
        &format!(
            r#"version 2
mission "parent" state="ready" report-to="agent/node.reporter" report-stalls=#false {{
  goal "Run occurrences until retirement."
  step "retire" {{
    agentless
    gate "retired" {{ field "state" "mission/parent" is "retired" }}
  }}
  schedule "cycle" {{
    every "1h"
    anchor "{anchor}"
    catch-up "latest"
    work {{ mission "watched-with-default-limit@{child_revision}"; workspace {:?} }}
  }}
}}"#,
            root.path().display().to_string()
        ),
        "retirement-parent",
    );
    let parent = fixture.start("parent", "schedule-parent");
    fixture.passes(4);
    tokio::task::yield_now().await; // Let the real schedule task arm its sleep.
    let schedule = format!("schedule/{}/cycle", parent.id);
    let armed = fixture
        .store
        .claims_for(&schedule, Some("schedule.occurrence-scheduled"))
        .unwrap();
    assert_eq!(armed.len(), 1);
    assert_eq!(armed[0].body["fields"]["occurrence"], 0);
    assert_eq!(
        armed[0].body["fields"]["scheduled_at_unix_ms"],
        anchor_ms.to_string()
    );
    let parent_view = fixture.store.mission_run(&parent.id).unwrap().unwrap();
    let retire = parent_view
        .steps
        .iter()
        .find(|step| step.step == "retire")
        .unwrap();
    assert!(retire.agentless);
    assert_eq!(retire.status, "working");
    assert!(retire.claimant.is_none());
    assert_eq!(
        fixture.reconciler.incremental.next_due(&parent.subject),
        None
    );

    let _quiet = Clock::at(&fixture.store, start() + 31 * MINUTE);
    fixture.passes(2);
    assert_eq!(fixture.report_claims(), 0);
    assert!(
        fixture
            .store
            .claims_for(&schedule, Some("schedule.occurrence-reached"))
            .unwrap()
            .is_empty()
    );
    let _anchor = Clock::at(&fixture.store, anchor_ms);
    tokio::time::advance(Duration::from_millis((60 * MINUTE) as u64)).await;
    tokio::task::yield_now().await;
    fixture.passes(3);
    let starts = fixture
        .store
        .claims_for(&schedule, Some("schedule.work-started"))
        .unwrap();
    assert_eq!(starts.len(), 1, "{starts:?}");
    let child = fixture
        .store
        .mission_run(starts[0].body["fields"]["mission_run"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(child.root_mission_run, parent.subject);
    assert!(
        fixture
            .reconciler
            .run_report(&child.subject)
            .unwrap()
            .unwrap()
            .stalls
    );
    let _child_quiet = Clock::at(&fixture.store, anchor_ms + 31 * MINUTE);
    fixture.passes(2);
    let reports = fixture.reports(REPORTER);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0]
            .tags
            .contains(&format!("mission-run:{}", child.subject))
    );
    assert!(reports[0].tags.contains(&"st3-run-report:stalled".into()));

    // Several missed anchors while the child is active never overlap it.
    let _missed = Clock::at(&fixture.store, anchor_ms + 4 * 60 * MINUTE);
    fixture.passes(2);
    assert_eq!(
        fixture
            .store
            .claims_for(&schedule, Some("schedule.work-started"))
            .unwrap()
            .len(),
        1
    );
    fixture
        .store
        .set_step_state(&Fixture::step(&child, "build"), "failed", None)
        .unwrap();
    fixture.passes(6);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::ZERO).await;
    tokio::task::yield_now().await;
    fixture.passes(3);
    let reached = fixture
        .store
        .claims_for(&schedule, Some("schedule.occurrence-reached"))
        .unwrap();
    let occurrences: Vec<_> = reached
        .iter()
        .map(|claim| claim.body["fields"]["occurrence"].as_u64().unwrap())
        .collect();
    assert_eq!(occurrences, [0, 4], "latest skips the missed occurrences");
    let starts = fixture
        .store
        .claims_for(&schedule, Some("schedule.work-started"))
        .unwrap();
    assert_eq!(starts.len(), 2);
    let second = fixture
        .store
        .mission_run(starts[1].body["fields"]["mission_run"].as_str().unwrap())
        .unwrap()
        .unwrap();
    fixture
        .store
        .request_mission_run_cancellation(&second.id, "cancel child fixture")
        .unwrap();
    fixture.passes(6);
    // Parent failure is reported even though its stalls were disabled.
    fixture
        .store
        .set_step_state(&Fixture::step(&parent, "retire"), "failed", None)
        .unwrap();
    fixture.passes(6);
    let cancelled_parent = fixture.start("parent", "cancelled-schedule-parent");
    fixture
        .store
        .request_mission_run_cancellation(&cancelled_parent.id, "cancel parent fixture")
        .unwrap();
    fixture.passes(6);
    let reports = fixture.reports(REPORTER);
    for (run, event) in [
        (&child, "stalled"),
        (&child, "failed"),
        (&second, "cancelled"),
        (&parent, "failed"),
        (&cancelled_parent, "cancelled"),
    ] {
        assert_eq!(
            reports
                .iter()
                .filter(|message| {
                    message
                        .tags
                        .contains(&format!("mission-run:{}", run.subject))
                        && message.tags.contains(&format!("st3-run-report:{event}"))
                })
                .count(),
            1,
            "{reports:?}"
        );
    }
    assert!(reports.iter().all(|message| {
        !(message
            .tags
            .contains(&format!("mission-run:{}", parent.subject))
            && message.tags.contains(&"st3-run-report:stalled".into()))
    }));
    // A restarted reconciler reads the same policy and sends no duplicate reports.
    fixture.reconciler = Reconciler::new(
        fixture.store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true);
    fixture.passes(2);
    assert_eq!(fixture.reports(REPORTER).len(), reports.len());
}
