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
            .set_mission_run_report(&run.subject, actor, to, stalled_after_ms, false, key)
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
            None,
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
