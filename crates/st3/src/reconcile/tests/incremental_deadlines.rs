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

/// A separate store, runtime, and driver directory for each deadline scenario.
struct GateFixture {
    store: Arc<Store>,
    runtime: Arc<FakeRuntime>,
    reconciler: Reconciler<FakeRuntime>,
    run: MissionRunView,
    stage: GateContext,
    workspace: tempfile::TempDir,
}

impl GateFixture {
    fn new() -> Self {
        let (store, runtime, mut reconciler, _) = stopping();
        let workspace = tempfile::tempdir().unwrap();
        reconciler.driver_state_dir = workspace.path().join("drivers");
        apply_source(
            &store,
            r#"version 2
mission "proof" state="ready" {
  goal "Prove the deadline."
  step "verify" { agentless }
}"#,
            "deadline-mission",
        );
        let run = start_proof_run(&store, "deadline-run");
        let view = step_of(&run, "verify");
        let stage = GateContext {
            subject: view.subject.clone(),
            name: view.step.clone(),
            started_at_unix_ms: START,
            attempt: view.attempt,
            run: run.subject.clone(),
            generation: run.generation.clone(),
            eval: false,
        };
        Self {
            store,
            runtime,
            reconciler,
            run,
            stage,
            workspace,
        }
    }

    fn mechanical(&self) -> Result<GateOutcome> {
        self.reconciler.run_mechanical(
            &self.stage,
            "deadline",
            "sleep 60",
            "node",
            self.workspace.path().to_str().unwrap(),
            &BTreeMap::new(),
            50,
        )
    }

    fn running(&self) {
        let runtime_id = self.runtime.starts.lock().unwrap().last().unwrap().clone();
        self.runtime.execs.lock().unwrap().insert(
            runtime_id.clone(),
            RuntimeObservation {
                runtime_id,
                terminal: false,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("deadline-runner-one".into()),
            },
        );
    }

    fn metric(&self) -> Result<Option<f64>> {
        let metric = crate::model::MetricSpec {
            name: "score".into(),
            direction: "minimize".into(),
            min_improvement: 0.0,
            source: MetricSource::Exec {
                command: "sleep 60".into(),
                host: "node".into(),
                workspace: self.workspace.path().to_string_lossy().into_owned(),
                environment: BTreeMap::new(),
                time_limit_ms: 50,
            },
        };
        self.reconciler.evaluate_exec_metric(
            &self.run,
            step_of(&self.run, "verify"),
            "loop-run/deadline",
            &metric,
            &BTreeMap::new(),
        )
    }
}

/// Settle the evaluation and its own writes, leaving only time able to invalidate it.
fn settle(reconciler: &Reconciler<FakeRuntime>, item: &str, evaluate: impl Fn() -> Result<()>) {
    for _ in 0..2 {
        reconciler.incremental.observe(&reconciler.store).unwrap();
        reconciler
            .recording_writes(|| reconciler.reconcile_item("deadline", item, false, &evaluate))
            .unwrap();
    }
    reconciler.incremental.observe(&reconciler.store).unwrap();
    assert!(!reconciler.incremental.needs(item, now_ms()));
}

#[test]
fn resume_verification_tracks_the_strict_timeout_with_unchanged_inputs() {
    for status in ["running", "starting"] {
        for already_expired in [true, false] {
            let _clock = Clock::at(START);
            let (store, _, reconciler, mut observation) = stopping();
            apply_source(
                &store,
                "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\" }",
                "deadline-agent",
            );
            let desired = store.desired_subjects().unwrap().remove(0);
            let member = desired.member.as_ref().unwrap();
            observation.runtime_id = member.runtime_id.clone();
            observation.status = status.into();
            let request = store
                .append_claim(&ClaimInput {
                    subject: desired.subject.clone(),
                    kind: "runtime.action.requested".into(),
                    actor: Some("person/example".into()),
                    fields: BTreeMap::from([("action".into(), Value::String("resume".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some("resume-deadline".into()),
                })
                .unwrap();
            let suspension = crate::suspension::Suspension {
                action: "resume".into(),
                phase: "verifying".into(),
                operation_id: request.id,
                requested_by: Some("person/example".into()),
                native_session_id: Some("native-example".into()),
                updated_at_unix_ms: START,
                requested_at_unix_ms: START,
                ..Default::default()
            };
            let evaluate = || {
                reconciler.reconcile_suspension(
                    &desired,
                    member,
                    Some(&observation),
                    None,
                    &suspension,
                )
            };
            let due = START + crate::suspension::VERIFY_TIMEOUT_MS + 1;
            if already_expired {
                smallclaims::store::set_thread_clock(Some(due));
                evaluate().unwrap();
            } else {
                let item = format!("member:{}", desired.subject);
                settle(&reconciler, &item, evaluate);
                smallclaims::store::set_thread_clock(Some(due - 1));
                assert!(!reconciler.incremental.needs(&item, now_ms()));
                evaluate().unwrap();
                assert!(
                    store
                        .operation_claim(&crate::suspension::resume_failed_key(
                            &suspension.operation_id,
                        ))
                        .unwrap()
                        .is_none()
                );
                smallclaims::store::set_thread_clock(Some(due));
                // The checker must allow this clock-only failure without a correction.
                reconciler
                    .recording_writes(|| {
                        reconciler.reconcile_item("member", &item, false, evaluate)
                    })
                    .unwrap();
            }
            let failure = store
                .operation_claim(&crate::suspension::resume_failed_key(
                    &suspension.operation_id,
                ))
                .unwrap()
                .unwrap();
            assert_eq!(failure.body["fields"]["code"], "native-session-unbound");
        }
    }
}

#[test]
fn mechanical_gate_tracks_its_request_deadline_before_the_next_poll() {
    for already_expired in [true, false] {
        let _clock = Clock::at(START);
        let fixture = GateFixture::new();
        assert!(matches!(
            fixture.mechanical().unwrap(),
            GateOutcome::Pending
        ));
        fixture.running();
        let evaluate = || fixture.mechanical().map(|_| ());
        let due = START + 50;
        if already_expired {
            smallclaims::store::set_thread_clock(Some(due));
            assert!(matches!(
                fixture.mechanical().unwrap(),
                GateOutcome::Broken(_)
            ));
        } else {
            let item = &fixture.run.subject;
            settle(&fixture.reconciler, item, evaluate);
            smallclaims::store::set_thread_clock(Some(due - 1));
            assert!(!fixture.reconciler.incremental.needs(item, now_ms()));
            smallclaims::store::set_thread_clock(Some(due));
            fixture
                .reconciler
                .recording_writes(|| {
                    fixture
                        .reconciler
                        .reconcile_item("mission", item, false, evaluate)
                })
                .unwrap();
        }
        assert_eq!(fixture.runtime.kills.lock().unwrap().len(), 1);
        let runner = gate_runners(&fixture.runtime).pop().unwrap();
        let result = fixture
            .store
            .latest_claim(&gate_runner_subject(&runner), Some("gate.result"))
            .unwrap()
            .unwrap();
        assert_eq!(result.body["fields"]["value"]["answer"], "broken");
    }
}

#[test]
fn exec_metric_tracks_its_request_deadline_before_the_next_poll() {
    for already_expired in [true, false] {
        let _clock = Clock::at(START);
        let fixture = GateFixture::new();
        assert!(fixture.metric().unwrap().is_none());
        fixture.running();
        let item = &fixture.run.subject;
        if !already_expired {
            settle(&fixture.reconciler, item, || fixture.metric().map(|_| ()));
            smallclaims::store::set_thread_clock(Some(START + 49));
            assert!(!fixture.reconciler.incremental.needs(item, now_ms()));
        }
        smallclaims::store::set_thread_clock(Some(START + 50));
        let error = fixture
            .reconciler
            .recording_writes(|| {
                fixture
                    .reconciler
                    .reconcile_item("mission", item, false, || fixture.metric().map(|_| ()))
            })
            .unwrap_err();
        assert!(error.to_string().contains("exceeded 50ms"), "{error:#}");
        assert_eq!(fixture.runtime.kills.lock().unwrap().len(), 1);
    }
}

#[test]
fn new_gate_and_metric_requests_record_the_exact_due_time_without_tokio() {
    let _clock = Clock::at(START);
    assert!(tokio::runtime::Handle::try_current().is_err());
    for metric in [false, true] {
        let fixture = GateFixture::new();
        // The due time is based on the accepted request, even with a later reader clock.
        smallclaims::store::set_thread_clock(Some(START + 25));
        let (result, due) = smallclaims::touched::record_due(|| {
            if metric {
                fixture.metric().map(|_| ())
            } else {
                fixture.mechanical().map(|_| ())
            }
        });
        result.unwrap();
        assert_eq!(due, Some(START + 50));
    }
}

#[test]
fn mechanical_recheck_records_its_due_time_without_tokio() {
    for already_expired in [true, false] {
        let _clock = Clock::at(START);
        assert!(tokio::runtime::Handle::try_current().is_err());
        let fixture = GateFixture::new();
        fixture.mechanical().unwrap();
        let runner = gate_runners(&fixture.runtime).pop().unwrap();
        exit_gate_runner(&fixture.runtime, &runner, Some(1), "");
        smallclaims::store::set_thread_clock(Some(START + 25));
        let (result, due) = smallclaims::touched::record_due(|| fixture.mechanical());
        assert!(matches!(result.unwrap(), GateOutcome::NotYet));
        let deadline = START + GATE_RECHECK_BASE_MS;
        assert_eq!(
            due,
            Some(deadline),
            "the result must schedule the second check"
        );
        let evaluate = || fixture.mechanical().map(|_| ());
        if !already_expired {
            let item = &fixture.run.subject;
            settle(&fixture.reconciler, item, evaluate);
            assert_eq!(
                fixture.reconciler.incremental.next_due(item),
                Some(deadline)
            );
            smallclaims::store::set_thread_clock(Some(deadline - 1));
            assert!(!fixture.reconciler.incremental.needs(item, now_ms()));
            smallclaims::store::set_thread_clock(Some(deadline));
            fixture.store.set_write_clock_at(deadline).unwrap();
            fixture
                .reconciler
                .recording_writes(|| {
                    fixture
                        .reconciler
                        .reconcile_item("mission", item, false, evaluate)
                })
                .unwrap();
        } else {
            smallclaims::store::set_thread_clock(Some(deadline));
            fixture.store.set_write_clock_at(deadline).unwrap();
            evaluate().unwrap();
        }
        assert_eq!(gate_runners(&fixture.runtime).len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn a_gate_timeout_wakes_while_its_runner_is_still_running() {
    use futures_util::FutureExt;
    let _clock = Clock::at(START);
    for metric in [false, true] {
        let fixture = GateFixture::new();
        let (result, due) = smallclaims::touched::record_due(|| {
            if metric {
                fixture.metric().map(|_| ())
            } else {
                fixture.mechanical().map(|_| ())
            }
        });
        result.unwrap();
        fixture.running();
        assert_eq!(due, Some(START + 50));
        assert!(
            fixture
                .reconciler
                .step_deadlines
                .lock()
                .unwrap()
                .values()
                .any(|at| *at == START + 50)
        );
        // Consume the request's notifications before advancing the timer alone.
        let _ = fixture.reconciler.notify.notified().now_or_never();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(49)).await;
        assert!(
            fixture
                .reconciler
                .notify
                .notified()
                .now_or_never()
                .is_none()
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            fixture
                .reconciler
                .notify
                .notified()
                .now_or_never()
                .is_some()
        );
        assert_eq!(
            fixture
                .runtime
                .execs
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .status,
            "running"
        );
    }
}
