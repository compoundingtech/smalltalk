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
    let frontier = store.changes_since(0, 0).unwrap();
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
    let index = store.index().unwrap();
    reconciler.reconcile_selected_intake("schedule", key, "schedule/cycle-0", true, || {
        panic!("clean skip ran")
    });
    assert_eq!(store.index().unwrap(), index);
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
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
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
    assert_eq!(reconciler.incremental.next_due("observer:"), None);
    assert_eq!(reconciler.incremental.next_due("schedule:"), None);
    stages(&reconciler, &desired);
    assert!(
        entries.take().is_empty(),
        "elapsed cached arms must not keep selecting no-ops"
    );
}

/// A real provider call remains pending after the observer's armed timer has fired.
struct PendingProvider {
    fail: bool,
    calls: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
impl ResourceProvider for PendingProvider {
    fn observe(
        &self,
        _request: ObservationRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            self.release.notified().await;
            if self.fail {
                anyhow::bail!("fixture connection reset");
            }
            Ok(crate::resource::ProviderObservation {
                facts: serde_json::json!({"repository_id":7,"issues":[]}),
                cursor: Some("completed".into()),
                next_check_unix_ms: now_ms() + 60_000,
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn an_elapsed_observer_deadline_is_consumed_while_the_real_provider_is_pending() {
    let clock = Clock::at(START);
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(START).unwrap();
    apply_source(&store, SCRIPTED_OBSERVER, "pending-provider");
    let entries = Arc::new(Entries::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_fault_injection(entries.clone())
    .with_resource_provider(Arc::new(PendingProvider {
        fail: false,
        calls: calls.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let revision = store
        .selected_desired_revision("observer/repo")
        .unwrap()
        .unwrap();
    let due = START + 10;
    reconciler
        .observer_deadlines
        .lock()
        .unwrap()
        .insert(format!("observer/repo:{revision}"), due);
    let desired = store.desired_subjects().unwrap();
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(reconciler.incremental.next_due("observer:"), Some(due));
    entries.take();
    // Let the real spawned arm install its sleep before advancing its deadline.
    tokio::task::yield_now().await;
    clock.set(due);
    tokio::time::advance(Duration::from_millis(10)).await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(reconciler.armed_observers.lock().unwrap().len(), 1);
    let mut completion = reconciler.event_notify.subscribe();
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(
        entries.take(),
        [("observer".into(), "observer/repo".into())]
    );
    assert_eq!(reconciler.incremental.next_due("observer:"), None);
    assert!(!reconciler.incremental.needs("observer:observer/repo", due));
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert!(entries.take().is_empty());
    assert!(
        reconciler
            .next_reconcile_deadline()
            .is_none_or(|at| at > due)
    );
    entries.take(); // Deadline discovery has its own fault-injection scopes.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!completion.has_changed().unwrap());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completion.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(reconciler.armed_observers.lock().unwrap().is_empty());
    assert_eq!(
        store.latest_actual_value("resource/repo").unwrap().unwrap()["facts"]["repository_id"],
        serde_json::json!(7)
    );
    let next = due + 60_000;
    assert_eq!(
        reconciler.observer_deadlines.lock().unwrap()[&format!("observer/repo:{revision}")],
        next
    );
    // The real completion's claim/notification selects a fresh read and arms the next cadence.
    entries.take(); // Completion's writer probes are distinct from item evaluation.
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(
        entries.take(),
        [("observer".into(), "observer/repo".into())]
    );
    assert_eq!(reconciler.incremental.next_due("observer:"), Some(next));
    tokio::task::yield_now().await;
    clock.set(next);
    tokio::time::advance(Duration::from_millis(60_000)).await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completion.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(reconciler.armed_observers.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_elapsed_schedule_deadline_is_consumed_until_its_real_timer_notifies() {
    let clock = Clock::at(START);
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(START).unwrap();
    let revision = scheduled_mission_revision(&store);
    let workspace = tempfile::tempdir().unwrap();
    let due = START + 10;
    let anchor = chrono::DateTime::from_timestamp_millis(due as i64)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    apply_source(
        &store,
        &format!(
            r#"version 2
schedule "later" {{ every "1h"; anchor "{anchor}"
    work {{ mission "scheduled-cycle@{revision}"; workspace "{}" }} }}
"#,
            workspace.path().display()
        ),
        "pending-schedule-timer",
    );
    let entries = Arc::new(Entries::default());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_fault_injection(entries.clone());
    let desired = store.desired_subjects().unwrap();
    reconciler.reconcile_schedules(&desired).unwrap();
    assert_eq!(reconciler.incremental.next_due("schedule:"), Some(due));
    tokio::task::yield_now().await;
    // Consume arming's append hints while the real timer is still sleeping.
    reconciler.reconcile_schedules(&desired).unwrap();
    assert_eq!(reconciler.incremental.next_due("schedule:"), Some(due));
    entries.take();
    clock.set(due);
    let mut completion = reconciler.event_notify.subscribe();
    // Wall clock has reached the deadline. Hold Tokio's actual timer until after these reads,
    // modeling an armed wake whose runnable task has not yet received executor time.
    reconciler.reconcile_schedules(&desired).unwrap();
    assert_eq!(
        entries.take(),
        [("schedule".into(), "schedule/later".into())]
    );
    assert_eq!(reconciler.incremental.next_due("schedule:"), None);
    assert!(!reconciler.incremental.needs("schedule:schedule/later", due));
    reconciler.reconcile_schedules(&desired).unwrap();
    assert!(entries.take().is_empty());
    assert_eq!(reconciler.armed_schedules.lock().unwrap().len(), 1);
    assert!(
        store
            .claims_for("schedule/later", Some("schedule.occurrence-reached"))
            .unwrap()
            .is_empty()
    );
    assert!(!completion.has_changed().unwrap());
    tokio::time::advance(Duration::from_millis(10)).await;
    tokio::time::timeout(Duration::from_secs(2), completion.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(reconciler.armed_schedules.lock().unwrap().is_empty());
    assert_eq!(
        store
            .claims_for("schedule/later", Some("schedule.occurrence-reached"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .pending_schedule_work_requests("schedule/later")
            .unwrap()
            .len(),
        1
    );
    // The completed timer still feeds the real work consumer, then the next occurrence.
    reconciler.reconcile_scheduled_work(&desired).unwrap();
    assert_eq!(
        store
            .claims_for("schedule/later", Some("schedule.work-started"))
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .pending_schedule_work_requests("schedule/later")
            .unwrap()
            .is_empty()
    );
    let active = store.active_mission_run_ids_for_origin("node").unwrap();
    assert_eq!(active.len(), 1);
    assert!(
        store
            .schedule_has_active_started_run("schedule/later")
            .unwrap()
    );
    reconciler.reconcile_schedules(&desired).unwrap();
    assert_eq!(reconciler.incremental.next_due("schedule:"), None);
    assert_eq!(
        store
            .claims_for("schedule/later", Some("schedule.occurrence-scheduled"))
            .unwrap()
            .len(),
        1,
        "a real running run blocks another occurrence"
    );
    // Use the existing mission evaluator, including agentless admission, step completion and
    // cleanup. Pass its COMPLETE active roster, never a dirty subset that could evict live caches.
    for _ in 0..8 {
        reconciler.evaluate_mission_runs().unwrap();
        if store
            .active_mission_run_ids_for_origin("node")
            .unwrap()
            .is_empty()
        {
            break;
        }
    }
    let finished = store.mission_run(&active[0]).unwrap().unwrap();
    assert_eq!(finished.status, "completed");
    assert_eq!(finished.phase, "terminal");
    assert_eq!(finished.steps[0].status, "completed");
    assert!(
        !store
            .schedule_has_active_started_run("schedule/later")
            .unwrap()
    );
    assert!(
        store
            .active_mission_run_ids_for_origin("node")
            .unwrap()
            .is_empty()
    );
    reconciler.reconcile_schedules(&desired).unwrap();
    assert_eq!(
        reconciler.incremental.next_due("schedule:"),
        Some(due + 3_600_000)
    );
    assert_eq!(
        store
            .claims_for("schedule/later", Some("schedule.occurrence-scheduled"))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn an_overlapping_timer_completion_survives_selected_evaluation_recording() {
    let _clock = Clock::at(START);
    let (store, reconciler, _) = fixture(2);
    let desired = store.desired_subjects().unwrap();
    stages(&reconciler, &desired);
    let item = "observer:observer/repo-0";
    // Force a real selected recorder to finish AFTER the completion's dirty mark. A generic
    // successful evaluation clears dirty; the intake token must retain this overlapping event.
    reconciler.reconcile_selected_intake("observer", item, "observer/repo-0", false, || {
        reconciler.incremental.intake_completed(item);
        Ok(())
    });
    assert!(reconciler.incremental.needs(item, START));
    assert!(
        !reconciler
            .incremental
            .needs("observer:observer/repo-1", START)
    );
    reconciler.reconcile_selected_intake("observer", item, "observer/repo-0", true, || Ok(()));
    assert!(!reconciler.incremental.needs(item, START));
    let new = "observer:observer/new";
    reconciler.reconcile_selected_intake("observer", new, "observer/new", false, || {
        reconciler.incremental.intake_completed(new);
        Ok(())
    });
    assert!(
        reconciler.incremental.needs(new, START),
        "first-arm completion before registration"
    );
}

#[tokio::test(start_paused = true)]
async fn unchanged_provider_failure_without_a_claim_still_selects_the_new_retry_deadline() {
    let clock = Clock::at(START);
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(START).unwrap();
    apply_source(&store, SCRIPTED_OBSERVER, "unchanged-provider-failure");
    let away = Store::open_memory("away").unwrap();
    apply_source(
        &away,
        &SCRIPTED_OBSERVER
            .replace("\"repo\"", "\"unrelated\"")
            .replace("resource/repo", "resource/unrelated"),
        "unrelated-foreign-observer",
    );
    store
        .import_replication("away", &away.export_replication(0).unwrap())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    )
    .skipping_unneeded(true)
    .with_resource_provider(Arc::new(PendingProvider {
        fail: true,
        calls: calls.clone(),
        entered: entered.clone(),
        release: release.clone(),
    }));
    let desired = store.desired_subjects().unwrap();
    let mut completion = reconciler.event_notify.subscribe();
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completion.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .claims_for("observer/repo", Some("observer.state"))
            .unwrap()
            .len(),
        1
    );
    assert!(
        reconciler
            .incremental
            .needs("observer:observer/repo", START)
    );
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(
        reconciler.incremental.next_due("observer:"),
        Some(START + 60_000)
    );
    tokio::task::yield_now().await;
    clock.set(START + 60_000);
    tokio::time::advance(Duration::from_millis(60_000)).await;
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(reconciler.incremental.next_due("observer:"), None);
    assert!(
        !reconciler
            .incremental
            .needs("observer:observer/repo", now_ms())
    );
    assert!(
        !reconciler
            .incremental
            .needs("observer:observer/unrelated", now_ms())
    );
    let before = store.index().unwrap();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completion.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        store.index().unwrap(),
        before,
        "unchanged failure publishes no new claim"
    );
    assert!(
        reconciler
            .incremental
            .needs("observer:observer/repo", now_ms()),
        "completion itself must select work without append hints"
    );
    assert!(
        !reconciler
            .incremental
            .needs("observer:observer/unrelated", now_ms()),
        "completion selects only the affected observer"
    );
    reconciler
        .reconcile_resource_observers(&desired, &desired)
        .unwrap();
    assert_eq!(
        reconciler.incremental.next_due("observer:"),
        Some(START + 120_000)
    );
    assert!(
        !reconciler
            .incremental
            .needs("observer:observer/repo", now_ms())
    );
}
