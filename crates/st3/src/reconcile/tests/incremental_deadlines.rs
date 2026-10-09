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

    fn launch_as(&self, incarnation: &str) {
        let runtime = self.runtime.clone();
        let incarnation = incarnation.to_owned();
        *self.runtime.before_observe_exec.lock().unwrap() = Some(Box::new(move || {
            let runtime_id = runtime.starts.lock().unwrap().last().unwrap().clone();
            runtime.execs.lock().unwrap().insert(
                runtime_id.clone(),
                RuntimeObservation {
                    runtime_id,
                    terminal: false,
                    status: "running".into(),
                    exit_code: None,
                    incarnation_id: Some(incarnation),
                },
            );
        }));
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
fn mechanical_gate_uses_an_on_time_exit_after_a_delayed_pass() {
    for receipt_at in [0, 20, 49] {
        for (reported_code, runtime_code, answer) in [
            (0, None, "pass"),
            (1, None, "not-yet"),
            (2, None, "broken"),
            (0, Some(0), "pass"),
            (1, Some(1), "not-yet"),
        ] {
            let _clock = Clock::at(START);
            let fixture = GateFixture::new();
            fixture.launch_as("runner-one");
            assert!(matches!(
                fixture.mechanical().unwrap(),
                GateOutcome::Pending
            ));
            let runner = gate_runners(&fixture.runtime).pop().unwrap();
            let operation = gate_runner_subject(&runner);
            fixture
                .store
                .set_write_clock_at(START + receipt_at)
                .unwrap();
            let completed = fixture
                .store
                .append_claim(&ClaimInput {
                    subject: operation.clone(),
                    kind: "runtime.observed".into(),
                    actor: Some(operation.clone()),
                    fields: BTreeMap::from([
                        ("status".into(), Value::String("exited".into())),
                        ("exit_code".into(), Value::from(reported_code)),
                        ("exit_signal".into(), Value::Null),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            assert_eq!(completed.accepted_at_unix_ms, START + receipt_at);
            exit_gate_runner(&fixture.runtime, &runner, runtime_code, "completed output");
            fixture
                .runtime
                .execs
                .lock()
                .unwrap()
                .get_mut(&runner.runtime_id)
                .unwrap()
                .incarnation_id = Some("runner-one".into());
            // The 50ms check ended on time; only its reconciliation was delayed.
            smallclaims::store::set_thread_clock(Some(START + 1_000));
            fixture.store.set_write_clock_at(START + 1_000).unwrap();
            let (outcome, due) = smallclaims::touched::record_due(|| fixture.mechanical());
            let outcome = outcome.unwrap();
            assert!(match answer {
                "pass" => matches!(outcome, GateOutcome::Pass),
                "not-yet" => matches!(outcome, GateOutcome::NotYet),
                _ => matches!(outcome, GateOutcome::Broken(_)),
            });
            assert!(fixture.runtime.kills.lock().unwrap().is_empty());
            let result = fixture
                .store
                .latest_claim(&operation, Some("gate.result"))
                .unwrap()
                .unwrap();
            assert_eq!(gate_check_answer(&result), answer);
            assert_eq!(result.body["fields"]["value"]["exit_code"], reported_code);
            assert_eq!(result.body["fields"]["value"]["output"], "completed output");
            assert!(!gate_result_reason(&result, "missing reason").contains("time limit"));
            if answer == "not-yet" {
                assert_eq!(
                    due,
                    Some(result.accepted_at_unix_ms + fixture.reconciler.gate_recheck_delay_ms(1))
                );
            }
        }
    }
}

#[test]
fn mechanical_gate_does_not_accept_unproved_or_late_completion() {
    for case in [
        "absent",
        "before-request",
        "at-deadline",
        "late",
        "wrong-actor",
        "daemon-observation",
        "wrong-check",
        "wrong-operation",
        "running-receipt",
        "missing-code",
        "text-code",
        "signal",
        "still-running",
        "contradictory-runtime",
        "wrong-runtime",
        "wrong-incarnation",
        "local-start-wrong-runtime",
        "multiple-launches",
        "mixed-launches",
        "foreign-origin",
        "foreign-batch-origin",
        "foreign-request",
        "repaired-request",
        "repaired",
    ] {
        let _clock = Clock::at(START);
        let fixture = GateFixture::new();
        if matches!(
            case,
            "wrong-incarnation" | "multiple-launches" | "mixed-launches"
        ) {
            fixture.launch_as("old-runner");
        }
        assert!(matches!(
            fixture.mechanical().unwrap(),
            GateOutcome::Pending
        ));
        let runner = gate_runners(&fixture.runtime).pop().unwrap();
        let operation = gate_runner_subject(&runner);
        let requested = fixture
            .store
            .latest_claim(&operation, Some("gate.requested"))
            .unwrap()
            .unwrap();
        let local_start = fixture
            .store
            .observations_for(&operation, "runtime.action.succeeded")
            .unwrap()
            .into_iter()
            .find(|claim| claim.body["fields"]["action"] == "start")
            .unwrap();
        assert_eq!(local_start.store_index, requested.store_index);
        // Explicit reader-only clock-reversal control: local log order must still
        // contribute incarnation and multiple-start evidence despite the earlier clock.
        fixture
            .store
            .connection
            .write()
            .execute(
                "UPDATE local_observations SET observed_at_unix_ms=?1
             WHERE subject=?2 AND kind='runtime.action.succeeded'",
                rusqlite::params![i64::try_from(START - 1).unwrap(), operation],
            )
            .unwrap();
        if case == "local-start-wrong-runtime" {
            fixture.store.connection.write().execute(
                "UPDATE local_observations SET body=json_set(body, '$.fields.runtime_id', 'wrong-runtime')
                 WHERE subject=?1 AND kind='runtime.action.succeeded' AND json_extract(body,'$.fields.action')='start'",
                [&operation],
            ).unwrap();
        }
        if case != "absent" {
            let at = match case {
                "at-deadline" => 50,
                "late" => 51,
                _ => 20,
            };
            fixture.store.set_write_clock_at(START + at).unwrap();
            let receipt_subject = match case {
                "wrong-check" => format!("{operation}/check/2"),
                "wrong-operation" => format!("{operation}-other"),
                _ => operation.clone(),
            };
            let completed = fixture
                .store
                .append_claim(&ClaimInput {
                    subject: receipt_subject.clone(),
                    kind: "runtime.observed".into(),
                    actor: match case {
                        "wrong-actor" => Some("person/example".into()),
                        "daemon-observation" => None,
                        _ => Some(receipt_subject),
                    },
                    fields: BTreeMap::from([
                        (
                            "status".into(),
                            Value::String(
                                if case == "running-receipt" {
                                    "running"
                                } else {
                                    "exited"
                                }
                                .into(),
                            ),
                        ),
                        (
                            "exit_code".into(),
                            match case {
                                "missing-code" => Value::Null,
                                _ => Value::from(0),
                            },
                        ),
                        (
                            "exit_signal".into(),
                            if case == "signal" {
                                Value::from(9)
                            } else {
                                Value::Null
                            },
                        ),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            if case == "text-code" {
                // Admission rejects this type. Mutate only this reader-negative fixture;
                // it does not assert that a malformed receipt could be admitted.
                fixture.store.connection.write().execute(
                    "UPDATE claims SET body=json_set(body, '$.fields.exit_code', '0') WHERE id=?1",
                    [&completed.id],
                ).unwrap();
            }
            if case == "before-request" {
                // Isolate original acceptance timing from writer ordering. This intentionally
                // mutated row is a reader control, not a signed admission fixture.
                fixture
                    .store
                    .connection
                    .write()
                    .execute(
                        "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                        rusqlite::params![(START - 1).to_string(), completed.id],
                    )
                    .unwrap();
            }
            if matches!(case, "foreign-origin" | "foreign-batch-origin") {
                // Isolate the reader's claim/batch provenance checks without asserting that
                // these deliberately inconsistent fixture rows would pass admission.
                let query = if case == "foreign-origin" {
                    "UPDATE claims SET origin='other' WHERE id=?1"
                } else {
                    "UPDATE batches SET origin='other' WHERE id=(SELECT batch_id FROM claims WHERE id=?1)"
                };
                fixture
                    .store
                    .connection
                    .write()
                    .execute(query, [&completed.id])
                    .unwrap();
            }
            if matches!(case, "repaired" | "repaired-request") {
                let id = if case == "repaired-request" {
                    fixture
                        .store
                        .latest_claim(&operation, Some("gate.requested"))
                        .unwrap()
                        .unwrap()
                        .id
                } else {
                    completed.id
                };
                fixture.store.connection.write().execute(
                    "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                     VALUES('record/repaired-exit','node',999999,'repaired-exit',0,X'00','repaired',?1,?2)",
                    rusqlite::params![id, (START + at).to_string()],
                ).unwrap();
            }
        }
        if case == "foreign-request" {
            fixture
                .store
                .connection
                .write()
                .execute(
                    "UPDATE claims SET origin='other' WHERE subject=?1 AND kind='gate.requested'",
                    [&operation],
                )
                .unwrap();
        }
        if case == "multiple-launches" {
            fixture.store.set_write_clock_at(START + 30).unwrap();
            fixture
                .store
                .append_claim(&ClaimInput {
                    subject: operation.clone(),
                    kind: "runtime.action.succeeded".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), Value::String("start".into())),
                        ("incarnation_id".into(), Value::String("new-runner".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        if case == "mixed-launches" {
            // Actor-bearing SystemLocal claims remain in the replicated graph. Changing
            // this fixture's actor column models legacy daemon storage only; it is a
            // mixed-reader control, not legacy-envelope admission qualification.
            let legacy = fixture
                .store
                .append_claim(&ClaimInput {
                    subject: operation.clone(),
                    kind: "runtime.action.succeeded".into(),
                    actor: Some(operation.clone()),
                    fields: BTreeMap::from([
                        ("action".into(), Value::String("start".into())),
                        ("incarnation_id".into(), Value::String("new-runner".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            fixture
                .store
                .connection
                .write()
                .execute("UPDATE claims SET actor=NULL WHERE id=?1", [&legacy.id])
                .unwrap();
            // Check ambiguity in the reader itself: a later runtime-incarnation
            // mismatch must not be the reason this mixed-source case times out.
            assert!(
                fixture
                    .store
                    .mechanical_gate_exit_receipt(&requested)
                    .unwrap()
                    .is_none()
            );
        }
        if case == "still-running" {
            fixture.running();
        } else {
            exit_gate_runner(
                &fixture.runtime,
                &runner,
                Some(if case == "contradictory-runtime" {
                    137
                } else {
                    0
                }),
                "completed output",
            );
            if case == "wrong-runtime" {
                fixture
                    .runtime
                    .execs
                    .lock()
                    .unwrap()
                    .get_mut(&runner.runtime_id)
                    .unwrap()
                    .runtime_id = "another-check".into();
            }
            if case == "multiple-launches" {
                fixture
                    .runtime
                    .execs
                    .lock()
                    .unwrap()
                    .get_mut(&runner.runtime_id)
                    .unwrap()
                    .incarnation_id = Some("new-runner".into());
            }
        }
        smallclaims::store::set_thread_clock(Some(START + 1_000));
        fixture.store.set_write_clock_at(START + 1_000).unwrap();
        assert!(
            matches!(fixture.mechanical().unwrap(), GateOutcome::Broken(_)),
            "{case}"
        );
        assert_eq!(
            fixture.runtime.kills.lock().unwrap().len(),
            usize::from(case == "still-running"),
            "{case}"
        );
        let result = fixture
            .store
            .latest_claim(&operation, Some("gate.result"))
            .unwrap()
            .unwrap();
        assert_eq!(gate_check_answer(&result), "broken", "{case}");
        assert!(
            gate_result_reason(&result, "missing reason").contains("time limit"),
            "{case}"
        );
    }
}

#[test]
fn mechanical_gate_accepts_an_exit_reported_before_start_success() {
    let _clock = Clock::at(START);
    let fixture = GateFixture::new();
    let runtime = fixture.runtime.clone();
    let store = fixture.store.clone();
    *fixture.runtime.before_observe_exec.lock().unwrap() = Some(Box::new(move || {
        let runner = gate_runners(&runtime).pop().unwrap();
        let operation = gate_runner_subject(&runner);
        store
            .append_claim(&ClaimInput {
                subject: operation.clone(),
                kind: "runtime.observed".into(),
                actor: Some(operation),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("exited".into())),
                    ("exit_code".into(), Value::from(1)),
                    ("exit_signal".into(), Value::Null),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        exit_gate_runner(&runtime, &runner, None, "fast exit");
    }));
    assert!(matches!(
        fixture.mechanical().unwrap(),
        GateOutcome::Pending
    ));
    let runner = gate_runners(&fixture.runtime).pop().unwrap();
    let operation = gate_runner_subject(&runner);
    let requested = fixture
        .store
        .latest_claim(&operation, Some("gate.requested"))
        .unwrap()
        .unwrap();
    let (completed, incarnation) = fixture
        .store
        .mechanical_gate_exit_receipt(&requested)
        .unwrap()
        .unwrap();
    let launch = fixture
        .store
        .observations_for(&operation, "runtime.action.succeeded")
        .unwrap()
        .into_iter()
        .find(|claim| claim.body["fields"]["action"] == "start")
        .unwrap();
    // A local start row sorts after the claim named by its after_store_index. It does
    // not consume another graph index, so this equality proves the intended fast order.
    assert_eq!(completed.store_index, launch.store_index);
    assert!(launch.id.starts_with("local-observation/"));
    assert_eq!(incarnation, Some(format!("{}-one", runner.runtime_id)));
    smallclaims::store::set_thread_clock(Some(START + 1_000));
    fixture.store.set_write_clock_at(START + 1_000).unwrap();
    assert!(matches!(fixture.mechanical().unwrap(), GateOutcome::NotYet));
    assert!(fixture.runtime.kills.lock().unwrap().is_empty());
}

#[test]
fn mechanical_gate_reader_excludes_local_starts_before_the_request() {
    let _clock = Clock::at(START);
    let fixture = GateFixture::new();
    let operation = gate_result_subject(
        &fixture.stage,
        "deadline",
        &serde_json::json!({
            "type": "mechanical", "command": "sleep 60", "host": "node",
            "workspace": fixture.workspace.path().to_str().unwrap(),
            "environment": {}, "time_limit_ms": 50,
        }),
    )
    .unwrap();
    let old = fixture
        .store
        .append_claim(&ClaimInput {
            subject: operation.clone(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String("start".into())),
                (
                    "incarnation_id".into(),
                    Value::String("older-runner".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    fixture.launch_as("current-runner");
    fixture.mechanical().unwrap();
    let requested = fixture
        .store
        .latest_claim(&operation, Some("gate.requested"))
        .unwrap()
        .unwrap();
    assert!(old.store_index < requested.store_index);
    fixture
        .store
        .append_claim(&ClaimInput {
            subject: operation.clone(),
            kind: "runtime.observed".into(),
            actor: Some(operation),
            fields: BTreeMap::from([
                ("status".into(), Value::String("exited".into())),
                ("exit_code".into(), Value::from(0)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    // A deliberately actor-bearing local start is not a daemon launch receipt.
    // This column mutation is only a reader control; actor-bearing SystemLocal
    // input normally goes into claims, as the mixed-launch control demonstrates.
    let extra = fixture
        .store
        .append_claim(&ClaimInput {
            subject: requested.subject.clone(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String("start".into())),
                (
                    "incarnation_id".into(),
                    Value::String("unrelated-runner".into()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    fixture
        .store
        .connection
        .write()
        .execute(
            "UPDATE local_observations SET actor='person/example' WHERE id=?1",
            [extra.id.rsplit('/').next().unwrap()],
        )
        .unwrap();
    let (_, incarnation) = fixture
        .store
        .mechanical_gate_exit_receipt(&requested)
        .unwrap()
        .unwrap();
    assert_eq!(incarnation.as_deref(), Some("current-runner"));
}

#[test]
fn mechanical_gate_reader_rejects_old_writer_history_inserted_late() {
    let _clock = Clock::at(START);
    let fixture = GateFixture::new();
    let operation = gate_result_subject(
        &fixture.stage,
        "deadline",
        &serde_json::json!({
            "type": "mechanical", "command": "sleep 60", "host": "node",
            "workspace": fixture.workspace.path().to_str().unwrap(),
            "environment": {}, "time_limit_ms": 50,
        }),
    )
    .unwrap();
    let exit = || ClaimInput {
        subject: operation.clone(),
        kind: "runtime.observed".into(),
        actor: Some(operation.clone()),
        fields: BTreeMap::from([
            ("status".into(), Value::String("exited".into())),
            ("exit_code".into(), Value::from(0)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    let old = fixture.store.append_claim(&exit()).unwrap();
    assert!(matches!(
        fixture.mechanical().unwrap(),
        GateOutcome::Pending
    ));
    let requested = fixture
        .store
        .latest_claim(&operation, Some("gate.requested"))
        .unwrap()
        .unwrap();
    assert_eq!(old.accepted_at_unix_ms, requested.accepted_at_unix_ms);
    assert!(old.store_index < requested.store_index);
    // Emulate re-insertion of admitted old writer history without changing its original
    // signed batch/sequence. A later local index cannot make it follow this request.
    let late_index = fixture.store.index().unwrap() + 1;
    fixture
        .store
        .connection
        .write()
        .execute(
            "UPDATE claims SET store_index=?1 WHERE id=?2",
            rusqlite::params![late_index, old.id],
        )
        .unwrap();
    assert!(late_index > requested.store_index);
    assert!(
        fixture
            .store
            .mechanical_gate_exit_receipt(&requested)
            .unwrap()
            .is_none()
    );
    let current = fixture.store.append_claim(&exit()).unwrap();
    let (completed, _) = fixture
        .store
        .mechanical_gate_exit_receipt(&requested)
        .unwrap()
        .unwrap();
    assert_eq!(completed.id, current.id);
    assert_eq!(completed.accepted_at_unix_ms, requested.accepted_at_unix_ms);
}

#[test]
fn mechanical_gate_reader_orders_positions_within_one_writer_batch() {
    for position in [0, 2] {
        let _clock = Clock::at(START);
        let fixture = GateFixture::new();
        fixture.mechanical().unwrap();
        let runner = gate_runners(&fixture.runtime).pop().unwrap();
        let operation = gate_runner_subject(&runner);
        let requested = fixture
            .store
            .latest_claim(&operation, Some("gate.requested"))
            .unwrap()
            .unwrap();
        let completed = fixture
            .store
            .append_claim(&ClaimInput {
                subject: operation.clone(),
                kind: "runtime.observed".into(),
                actor: Some(operation),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("exited".into())),
                    ("exit_code".into(), Value::from(0)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        // Reader-only metadata fixture: isolate canonical replica position within a single
        // batch. This does not assert admission of a synthetic mixed-actor envelope.
        let connection = fixture.store.connection.write();
        connection
            .execute(
                "UPDATE claims SET batch_id=?1 WHERE id=?2",
                rusqlite::params![requested.batch_id, completed.id],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM replica_records WHERE claim_id IN (?1,?2)",
                rusqlite::params![requested.id, completed.id],
            )
            .unwrap();
        for (id, slot) in [(&requested.id, 1), (&completed.id, position)] {
            connection.execute(
                "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,claim_id,updated_at_unix_ms)
                 VALUES(?1,'node',999999,'position-fixture',?2,X'00','valid',?3,?4)",
                rusqlite::params![format!("record/position-{slot}"),slot,id,START.to_string()],
            ).unwrap();
        }
        drop(connection);
        let found = fixture
            .store
            .mechanical_gate_exit_receipt(&requested)
            .unwrap();
        assert_eq!(found.is_some(), position > 1);
        if let Some((receipt, _)) = found {
            assert_eq!(receipt.id, completed.id);
        }
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
