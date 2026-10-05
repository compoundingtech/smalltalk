//! Clock-only transitions must be both scheduled and visible to the correction checker.
use super::*;

const START: u128 = 1_800_000_000_000;

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

fn stopping() -> (
    Arc<Store>,
    Arc<FakeRuntime>,
    Reconciler<FakeRuntime>,
    RuntimeObservation,
) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    store.set_write_clock_at(START).unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let observation = RuntimeObservation {
        runtime_id: "node.worker".into(),
        terminal: true,
        status: "running".into(),
        exit_code: None,
        incarnation_id: Some("worker-one".into()),
    };
    (store, runtime, reconciler, observation)
}

fn stop(reconciler: &Reconciler<FakeRuntime>, observation: &RuntimeObservation) -> Result<()> {
    reconciler
        .reconcile_runtime_stop(
            "agent/node.worker",
            &observation.runtime_id,
            true,
            observation.incarnation_id.as_deref(),
            50,
            Some(observation),
        )
        .map(|_| ())
}

#[test]
fn a_new_stop_tracks_its_deadline_before_the_first_poll() {
    let _clock = Clock::at(START);
    let (_, _, reconciler, observation) = stopping();
    let (result, due) = smallclaims::touched::record_due(|| stop(&reconciler, &observation));
    result.unwrap();
    assert_eq!(due, Some(START + 50));
    assert_eq!(reconciler.next_deadline(), Some(START + 50));
}

#[test]
fn a_failed_terminate_still_tracks_the_kill_deadline() {
    let _clock = Clock::at(START);
    let (_, runtime, reconciler, observation) = stopping();
    runtime
        .failed_stops
        .lock()
        .unwrap()
        .insert(observation.runtime_id.clone());
    let (result, due) = smallclaims::touched::record_due(|| stop(&reconciler, &observation));
    assert!(result.is_err());
    assert_eq!(due, Some(START + 50));
    assert_eq!(reconciler.next_deadline(), Some(START + 50));
}

#[test]
fn every_incremental_section_contributes_its_due_time_to_the_daemon_wake() {
    let _clock = Clock::at(START);
    for item in [
        "stop:worker",
        "away:worker",
        "member:worker",
        "mission-run/worker",
        "subscription:worker",
        "wake:worker",
        "capacity:worker",
    ] {
        let (_, _, reconciler, _) = stopping();
        let reconciler = reconciler.skipping_unneeded(true);
        reconciler
            .incremental
            .evaluated(item, BTreeSet::new(), Some(START + 10));
        assert_eq!(reconciler.next_deadline(), Some(START + 10), "{item}");
    }
}

#[test]
fn a_stop_deadline_passed_between_incremental_evaluations_kills_once() {
    let _clock = Clock::at(START);
    let (store, runtime, reconciler, observation) = stopping();
    reconciler.incremental.observe(&store).unwrap();
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", true, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    // Consume the request's write, then settle while the clock is still before its deadline.
    reconciler.incremental.observe(&store).unwrap();
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", true, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    assert!(
        !reconciler
            .incremental
            .needs("stop:agent/node.worker", START + 49)
    );
    smallclaims::store::set_thread_clock(Some(START + 50));
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", true, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    assert_eq!(*runtime.kills.lock().unwrap(), ["node.worker"]);
    let deadline = store
        .latest_observation("agent/node.worker", "runtime.action.deadline-reached")
        .unwrap()
        .unwrap();
    let killed = store
        .latest_observation("agent/node.worker", "runtime.action.succeeded")
        .unwrap()
        .unwrap();
    assert_eq!(killed.body["evidence"], serde_json::json!([deadline.id]));
    reconciler.incremental.observe(&store).unwrap();
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", true, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    assert_eq!(runtime.kills.lock().unwrap().len(), 1);
}

#[test]
fn a_full_evaluation_uses_one_clock_sample_when_a_tracked_deadline_crosses() {
    let _clock = Clock::at(START);
    let (store, runtime, reconciler, observation) = stopping();
    reconciler.incremental.observe(&store).unwrap();
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", false, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    reconciler.incremental.observe(&store).unwrap();
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", false, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    smallclaims::store::set_thread_clock(Some(START + 49));
    assert!(
        !reconciler
            .incremental
            .needs("stop:agent/node.worker", now_ms())
    );
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", false, || {
                smallclaims::store::set_thread_clock(Some(START + 50));
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    assert!(runtime.kills.lock().unwrap().is_empty());
    assert_eq!(
        now_ms(),
        START + 50,
        "the reader snapshot ended with the item"
    );
    assert!(
        reconciler
            .incremental
            .needs("stop:agent/node.worker", now_ms())
    );
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", true, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
    assert_eq!(*runtime.kills.lock().unwrap(), ["node.worker"]);
}

#[test]
fn a_stop_item_observing_its_runtime_across_the_deadline_uses_one_clock_sample() {
    let _clock = Clock::at(START);
    let (store, runtime, reconciler, mut observation) = stopping();
    observation.terminal = false;
    let workspace = tempfile::tempdir().unwrap();
    apply_source(
        &store,
        &format!(
            "version 2\nexec \"worker\" {{ workspace {:?}; command \"sleep 60\"; shutdown-timeout \"50ms\" }}",
            workspace.path().to_str().unwrap(),
        ),
        "declare-worker",
    );
    let desired = store.desired_subjects().unwrap().remove(0);
    observation.runtime_id = desired.member.as_ref().unwrap().runtime_id.clone();
    runtime
        .execs
        .lock()
        .unwrap()
        .insert(observation.runtime_id.clone(), observation.clone());
    reconciler
        .record_once(
            &desired.subject,
            "runtime.observed",
            member_fields(
                desired.member.as_ref().unwrap(),
                "running",
                Some("worker-one"),
                true,
            ),
        )
        .unwrap();
    apply_source(
        &store,
        &format!("version 2\nstop {:?}", desired.subject),
        "stop-worker",
    );
    let desired = store.desired_subjects().unwrap().remove(0);
    let item = format!("stop:{}", desired.subject);
    let mut errors = Vec::new();
    let pass = |errors: &mut Vec<String>| {
        reconciler.recording_writes(|| {
            reconciler.reconcile_stop_item(&desired, &item, false, None, &BTreeSet::new(), errors);
        })
    };
    reconciler.incremental.observe(&store).unwrap();
    pass(&mut errors);
    reconciler.incremental.observe(&store).unwrap();
    pass(&mut errors);
    smallclaims::store::set_thread_clock(Some(START + 49));
    assert!(!reconciler.incremental.needs(&item, now_ms()));
    *runtime.observe_at.lock().unwrap() = Some(START + 50);
    pass(&mut errors);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(runtime.kills.lock().unwrap().is_empty());
    assert_eq!(now_ms(), START + 50);
    assert!(reconciler.incremental.needs(&item, now_ms()));
    pass(&mut errors);
    assert_eq!(*runtime.kills.lock().unwrap(), [observation.runtime_id]);
}

#[test]
#[should_panic(expected = "an incremental pass would have missed a write")]
fn an_untracked_expired_deadline_is_still_a_correction() {
    let _clock = Clock::at(START);
    let (_, _, reconciler, observation) = stopping();
    stop(&reconciler, &observation).unwrap();
    // Deliberately omit the dependency: advancing the clock must not excuse this write.
    reconciler
        .incremental
        .evaluated("stop:agent/node.worker", BTreeSet::new(), None);
    smallclaims::store::set_thread_clock(Some(START + 50));
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", false, || {
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
}

#[test]
#[should_panic(expected = "an incremental pass would have missed a write")]
fn recording_a_due_time_during_an_unneeded_evaluation_does_not_excuse_its_write() {
    let _clock = Clock::at(START);
    let (_, _, reconciler, observation) = stopping();
    reconciler
        .incremental
        .evaluated("stop:agent/node.worker", BTreeSet::new(), None);
    reconciler
        .recording_writes(|| {
            reconciler.reconcile_item("stop", "stop:agent/node.worker", false, || {
                smallclaims::touched::note_due(START);
                stop(&reconciler, &observation)
            })
        })
        .unwrap();
}
