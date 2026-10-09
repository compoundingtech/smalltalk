//! Component selection controls. These budgets are not a production full-pass denominator.
use super::*;
use smallclaims::sqlite::work::SqliteWorkScope;

const START: u128 = 1_900_000_000_000;

struct Clock;
impl Clock {
    fn at(at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        Self
    }
    fn set(&self, at: u128) {
        smallclaims::store::set_thread_clock(Some(at));
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

#[derive(Default)]
struct Entries(Mutex<Vec<(String, String)>>);
impl FaultInjection for Entries {
    fn fault(&self, scope: &str, subject: &str) -> Option<String> {
        self.0.lock().unwrap().push((scope.into(), subject.into()));
        None
    }
}
impl Entries {
    fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

fn fixture(count: usize) -> (Arc<Store>, Reconciler<FakeRuntime>, Arc<Entries>) {
    // Every declaration was genuinely authored on away; node must inspect but never execute it.
    let store = Arc::new(Store::open_memory("away").unwrap());
    store.set_write_clock_at(START).unwrap();
    let revision = scheduled_mission_revision(&store);
    let mut source = "version 2\n".to_owned();
    for n in 0..count {
        source.push_str(&format!(r#"
resource "repo-{n}" {{ kind "vcs.repository" }}
observer "repo-{n}" {{ resource "resource/repo-{n}"; provider "github.repository"; locator "example/repo-{n}"; field "issues" }}
schedule "cycle-{n}" {{ host "away"; every "7d"; anchor "2030-01-01T00:00:00Z"
    work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp" }} }}
"#));
    }
    if count != 0 {
        apply_source(&store, &source, "foreign-intake");
    }
    let entries = Arc::new(Entries::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_fault_injection(entries.clone());
    (store, reconciler, entries)
}

fn stages(reconciler: &Reconciler<FakeRuntime>, desired: &[DesiredSubject]) {
    reconciler
        .reconcile_resource_observers(desired, desired)
        .unwrap();
    reconciler.reconcile_schedules(desired).unwrap();
    reconciler.reconcile_scheduled_work(desired).unwrap();
}

#[test]
fn unchanged_intake_component_sql_does_not_grow_with_foreign_inventory() {
    let _clock = Clock::at(START);
    let mut costs = Vec::new();
    for count in [8, 64] {
        let (store, reconciler, entries) = fixture(count);
        let desired = store.desired_subjects().unwrap();
        stages(&reconciler, &desired);
        assert_eq!(entries.take().len(), count * 3);
        let scope = SqliteWorkScope::start();
        stages(&reconciler, &desired);
        let cost = scope.finish();
        assert!(
            entries.take().is_empty(),
            "no observer/schedule evaluation on clean input"
        );
        assert!(
            cost.statements <= 3 && cost.vm_steps <= 200 && cost.fullscan_steps == 0,
            "{cost:?}"
        );
        costs.push(cost);
        assert!(reconciler.runtime.starts.lock().unwrap().is_empty());
    }
    assert_eq!(
        costs[0], costs[1],
        "same three empty-feed statements, not roster evaluation: {costs:?}"
    );
}

#[test]
fn unrelated_traffic_stays_clean_but_exact_observer_and_schedule_writes_select_work() {
    let _clock = Clock::at(START);
    let (store, reconciler, entries) = fixture(8);
    let desired = store.desired_subjects().unwrap();
    stages(&reconciler, &desired);
    entries.take();
    store
        .put_document("doc/unrelated", b"unrelated", &None, "unrelated")
        .unwrap();
    stages(&reconciler, &desired);
    assert!(entries.take().is_empty());
    // A resource result is read by local observers; these foreign observers read only their
    // declaration origin. Exact declaration-subject traffic nevertheless selects both stages.
    for subject in ["observer/repo-2", "schedule/cycle-3"] {
        let (kind, field, value) = if subject.starts_with("observer/") {
            ("observer.state", "state", "healthy")
        } else {
            ("runtime.reconcile-decision", "decision", "wait")
        };
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: None,
                fields: BTreeMap::from([(field.into(), Value::String(value.into()))]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    stages(&reconciler, &desired);
    assert_eq!(
        entries.take().into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ("observer".into(), "observer/repo-2".into()),
            ("schedule".into(), "schedule/cycle-3".into()),
            ("schedule-work".into(), "schedule/cycle-3".into()),
        ])
    );
}

#[test]
fn fresh_subscription_move_and_physical_removal_select_both_observer_contexts() {
    let _clock = Clock::at(START);
    let (store, reconciler, entries) = fixture(2);
    apply_source(
        &store,
        r#"version 2
agent "target" { host "away"; workspace "/tmp"; command "true" }
subscription "watch" { observer "observer/repo-0"; to "agent/target"; on "issues"; delivery "message" }
"#,
        "subscribe",
    );
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    entries.take();
    apply_source(
        &store,
        r#"version 2
subscription "watch" { observer "observer/repo-1"; to "agent/target"; on "issues"; delivery "message" }
"#,
        "move-subscription",
    );
    let changed = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&changed, &changed)
        .unwrap();
    assert_eq!(
        entries.take().into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ("observer".into(), "observer/repo-0".into()),
            ("observer".into(), "observer/repo-1".into())
        ])
    );
    // A reader-side deletion fixture at unchanged append frontiers, matching the raw desired
    // sensor obligation. No admission/replication proof is inferred from this mutation.
    let frontier = store.changes_since(u64::MAX, i64::MAX).unwrap();
    store
        .connection
        .write()
        .execute("DELETE FROM desired WHERE subject='subscription/watch'", [])
        .unwrap();
    let after = store.changes_since(frontier.index, frontier.local).unwrap();
    assert!(after.changes.is_empty());
    let deleted = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&deleted, &deleted)
        .unwrap();
    assert_eq!(
        entries.take(),
        [("observer".into(), "observer/repo-1".into())]
    );
    assert!(
        !reconciler
            .incremental
            .reads_of("observer:observer/repo-0")
            .is_empty()
    );
}

#[test]
fn deadlines_full_pass_restart_and_failed_full_evaluation_do_not_disappear() {
    let clock = Clock::at(START);
    let (store, reconciler, entries) = fixture(2);
    let desired = store.desired_subjects().unwrap();
    stages(&reconciler, &desired);
    entries.take();
    let key = "schedule:schedule/cycle-0";
    let reads = reconciler.incremental.reads_of(key);
    reconciler
        .incremental
        .evaluated(key, reads.clone(), Some(START + 10));
    clock.set(START + 9);
    stages(&reconciler, &desired);
    assert!(entries.take().is_empty());
    clock.set(START + 10);
    stages(&reconciler, &desired);
    assert_eq!(
        entries.take(),
        [("schedule".into(), "schedule/cycle-0".into())]
    );
    // A full evaluation that fails used to leave an older clean evaluation. It must stay needed.
    reconciler.reconcile_selected_intake("schedule", key, "schedule/cycle-0", false, || {
        anyhow::bail!("evaluated failure")
    });
    assert!(reconciler.incremental.needs(key, START + 10));
    let failed = store.open_reconcile_faults("away").unwrap();
    assert!(failed.contains_key(&("schedule/cycle-0".into(), "schedule".into())));
    assert_eq!(reconciler.incremental.reads_of(key), reads);
    entries.take();
    stages(&reconciler, &desired);
    assert!(
        entries
            .take()
            .contains(&("schedule".into(), "schedule/cycle-0".into()))
    );
    clock.set(START + crate::incremental::FULL_PASS_INTERVAL_MS);
    stages(&reconciler, &desired);
    assert_eq!(entries.take().len(), 6);
    let restarted = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_fault_injection(entries.clone());
    stages(&restarted, &desired);
    assert_eq!(entries.take().len(), 6);
}

#[test]
fn a_clean_skip_keeps_a_fault_and_a_selected_failure_cannot_publish_recovery() {
    let _clock = Clock::at(START);
    let (store, reconciler, entries) = fixture(1);
    let desired = store.desired_subjects().unwrap();
    stages(&reconciler, &desired);
    entries.take();
    let key = "schedule:schedule/cycle-0";
    reconciler
        .record_fault(
            "schedule/cycle-0",
            "schedule",
            Err(anyhow::anyhow!("prior fault")),
        )
        .unwrap();
    // Consume reporting traffic and explicitly restore the already successful evaluation.
    reconciler.incremental.observe(&store).unwrap();
    let reads = reconciler.incremental.reads_of(key);
    reconciler.incremental.evaluated(key, reads, None);
    let index = store.index();
    reconciler.reconcile_selected_intake("schedule", key, "schedule/cycle-0", true, || {
        panic!("clean skip ran")
    });
    assert_eq!(store.index(), index);
    assert!(
        store
            .open_reconcile_faults("away")
            .unwrap()
            .contains_key(&("schedule/cycle-0".into(), "schedule".into()))
    );
    assert!(entries.take().is_empty());
    // Existing owned-set controls cover full stale/incarnation authority. At this consumer
    // boundary a selected failure cannot count as success, recovery, or a cached evaluation.
    reconciler.incremental.touch(key);
    let calls = std::cell::Cell::new(0);
    reconciler.reconcile_selected_intake("schedule", key, "schedule/cycle-0", true, || {
        calls.set(calls.get() + 1);
        anyhow::bail!("selected evaluation failed");
    });
    assert_eq!(calls.get(), 1);
    assert!(reconciler.incremental.needs(key, START));
    assert!(
        store
            .open_reconcile_faults("away")
            .unwrap()
            .contains_key(&("schedule/cycle-0".into(), "schedule".into()))
    );
}

#[test]
fn an_owned_schedule_selected_after_replacement_uses_the_fresh_captured_declaration_guard() {
    use crate::store::owned_sets::{Options, Source};
    let _clock = Clock::at(START);
    let (store, reconciler, entries) = fixture(0);
    let revision = store
        .mission_spec("scheduled-cycle", None)
        .unwrap()
        .unwrap()
        .revision;
    let publish = |sequence: u64, anchor: &str| {
        let intent = parse_intent(
            &format!(
                r#"version 2
schedule "owned" {{ host "away"; every "7d"; anchor "{anchor}"
    work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp" }} }}
"#
            ),
            "away",
        )
        .unwrap();
        let mut options = Options {
            set: "intake".into(),
            source: Source {
                repository: "acme/intake".into(),
                r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"),
                sequence,
            },
            expected_set: store
                .owned_sets()
                .unwrap()
                .first()
                .map_or("absent".into(), |view| view.revision.clone()),
            rollout: None,
            adopt: BTreeSet::new(),
            allow_empty: false,
            confirm_retire: None,
            expected_subjects: BTreeMap::new(),
        };
        options.expected_subjects = store
            .owned_set_preview(&intent, &options)
            .unwrap()
            .expected_subjects;
        store
            .apply_owned_set(
                &intent,
                &options,
                &format!("owned-{sequence}"),
                "person/operator",
            )
            .unwrap();
    };
    publish(10, "2030-01-01T00:00:00Z");
    let captured = store.desired_subjects().unwrap();
    let old = captured
        .iter()
        .find(|item| item.subject == "schedule/owned")
        .unwrap();
    reconciler.reconcile_schedules(&captured).unwrap();
    assert!(store.owned_desired_guard(old).is_ok());
    entries.take();
    publish(20, "2031-01-01T00:00:00Z");
    assert_eq!(
        store.owned_desired_guard(old).unwrap_err().code,
        "stale-set-member"
    );
    reconciler.reconcile_schedules(&captured).unwrap();
    assert_eq!(
        entries.take(),
        [("schedule".into(), "schedule/owned".into())]
    );
    assert!(
        reconciler
            .incremental
            .needs("schedule:schedule/owned", START)
    );
    assert!(
        store
            .claims_for("schedule/owned", Some("schedule.occurrence-scheduled"))
            .unwrap()
            .is_empty()
    );
    assert!(reconciler.runtime.starts.lock().unwrap().is_empty());
    assert!(
        store
            .open_reconcile_faults("away")
            .unwrap()
            .contains_key(&("schedule/owned".into(), "schedule".into()))
    );
    let fresh = store.desired_subjects().unwrap();
    reconciler.reconcile_schedules(&fresh).unwrap();
    assert!(
        !store
            .open_reconcile_faults("away")
            .unwrap()
            .contains_key(&("schedule/owned".into(), "schedule".into()))
    );
}

#[test]
fn local_observer_and_schedule_arm_clock_dependencies_before_cached_timer_returns() {
    let clock = Clock::at(START);
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(START).unwrap();
    let revision = scheduled_mission_revision(&store);
    let due = START + 86_400_000;
    let at = chrono::DateTime::from_timestamp_millis(due as i64)
        .unwrap()
        .to_rfc3339();
    apply_source(
        &store,
        &format!(
            r#"{SCRIPTED_OBSERVER}
schedule "later" {{ at "{at}"; work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp" }} }}
"#
        ),
        "local-deadlines",
    );
    let observer_revision = store
        .selected_desired_revision("observer/repo")
        .unwrap()
        .unwrap();
    let entries = Arc::new(Entries::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_fault_injection(entries.clone());
    reconciler
        .observer_deadlines
        .lock()
        .unwrap()
        .insert(format!("observer/repo:{observer_revision}"), due);
    let desired = store.desired_subjects().unwrap();
    stages(&reconciler, &desired);
    // No async executor is installed. Pretend the already armed timers remain sleeping, so
    // the second real evaluation takes the existing duplicate-timer early returns.
    let schedule_revision = store
        .selected_desired_revision("schedule/later")
        .unwrap()
        .unwrap();
    reconciler
        .armed_schedules
        .lock()
        .unwrap()
        .insert(format!("schedule/later:{schedule_revision}:0"));
    reconciler.armed_observers.lock().unwrap().insert(format!(
        "observer/repo:{observer_revision}:scheduled:default:fixture"
    ));
    reconciler.incremental.touch("observer:observer/repo");
    stages(&reconciler, &desired);
    assert_eq!(reconciler.incremental.next_due("observer:"), Some(due));
    assert_eq!(reconciler.incremental.next_due("schedule:"), Some(due));
    entries.take();
    clock.set(START + 1);
    stages(&reconciler, &desired);
    assert!(entries.take().is_empty());
    clock.set(due);
    // First mark the periodic full checks as already done, isolating due-item selection.
    for section in ["observer", "schedule", "schedule-work"] {
        reconciler.incremental.take_full_pass(section, due);
    }
    stages(&reconciler, &desired);
    assert_eq!(
        entries.take().into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ("observer".into(), "observer/repo".into()),
            ("schedule".into(), "schedule/later".into()),
        ])
    );
}
