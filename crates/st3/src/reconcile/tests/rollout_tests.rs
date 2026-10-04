use super::*;
use crate::rollout::{self, Policy};
use crate::store::owned_sets::{Options, Source};
use serde_json::json;

#[derive(Default)]
struct Runtime {
    observation: Mutex<Option<RuntimeObservation>>,
    starts: Mutex<Vec<MemberSpec>>,
    stops: Mutex<Vec<String>>,
}
impl RuntimeControl for Runtime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        Ok(self
            .observation
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .collect())
    }
    fn observe_exec(&self, _: &str) -> Result<Option<RuntimeObservation>> {
        Ok(self.observation.lock().unwrap().clone())
    }
    fn start(&self, member: &MemberSpec) -> Result<()> {
        let mut starts = self.starts.lock().unwrap();
        starts.push(member.clone());
        *self.observation.lock().unwrap() = Some(RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: member.terminal,
            status: "running".into(),
            incarnation_id: Some(format!("replacement-{}", starts.len())),
            exit_code: None,
        });
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, expected: Option<&str>) -> Result<()> {
        let mut state = self.observation.lock().unwrap();
        let observed = state.as_mut().unwrap();
        anyhow::ensure!(
            observed.incarnation_id.as_deref() == expected,
            "stale incarnation"
        );
        self.stops.lock().unwrap().push(expected.unwrap().into());
        observed.status = "exited".into();
        Ok(())
    }
    fn kill(&self, id: &str, terminal: bool, expected: Option<&str>) -> Result<()> {
        self.stop(id, terminal, expected)
    }
    fn remove(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}
struct Seat {
    root: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
}
const SUBJECT: &str = "agent/garden/orchard";
impl Seat {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), "amber").unwrap());
        let seat = Self {
            root,
            store,
            runtime: Arc::new(Runtime::default()),
        };
        seat.publish(1, "first", None, false);
        let desired = seat.desired();
        let member = desired.member.unwrap();
        let token = seat.store.selected_desired_token(SUBJECT).unwrap().unwrap();
        *seat.runtime.observation.lock().unwrap() = Some(RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: member.terminal,
            status: "running".into(),
            incarnation_id: Some("original-1".into()),
            exit_code: None,
        });
        seat.append("runtime.observed",json!({"status":"running","runtime_id":member.runtime_id,"host":"amber","terminal":member.terminal,"incarnation_id":"original-1"}));
        seat.append(
            "runtime.action.succeeded",
            json!({"action":"start","desired_token":token,"incarnation_id":"original-1"}),
        );
        seat.binding("original-1", "native-one");
        seat.busy(true);
        seat
    }
    fn append(&self, kind: &str, fields: Value) {
        self.store
            .append_claim(&ClaimInput {
                subject: SUBJECT.into(),
                kind: kind.into(),
                actor: Some(SUBJECT.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    fn binding(&self, incarnation: &str, session: &str) {
        self.append(
            "harness.session-file",
            json!({"harness":"claude","session_id":session,"incarnation_id":incarnation}),
        );
    }
    fn busy(&self, busy: bool) {
        self.append("harness.observed",json!({"state":if busy{"working"}else{"idle"},"driver":"claude","incarnation_id":"original-1","quiescent":!busy,"blocking":if busy{vec!["turn-in-flight"]}else{vec![]} }));
    }
    fn publish(&self, sequence: u64, model: &str, policy: Option<Policy>, retire: bool) {
        let source = if retire {
            "version 2".into()
        } else {
            format!(
                "version 2\nagent \"garden/orchard\" {{ host \"amber\"; workspace {:?}; harness \"claude\" {{ model {model:?}; }} }}",
                self.root.path().display().to_string()
            )
        };
        let input = crate::graph::parse_owned_set_intent(&source, "amber").unwrap();
        let mut options = Options {
            set: "garden".into(),
            source: Source {
                repository: "acme/garden".into(),
                r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"),
                sequence,
            },
            expected_set: self
                .store
                .owned_sets()
                .unwrap()
                .first()
                .map_or("absent".into(), |s| s.revision.clone()),
            rollout: policy,
            adopt: Default::default(),
            allow_empty: retire,
            confirm_retire: None,
            expected_subjects: Default::default(),
        };
        let preview = self.store.owned_set_preview(&input, &options).unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        options.confirm_retire = Some(preview.digest);
        options.expected_subjects = preview.expected_subjects;
        self.store
            .apply_owned_set(
                &input,
                &options,
                &format!("publish-{sequence}"),
                "person/operator",
            )
            .unwrap();
    }
    fn work(&self) -> crate::model::MissionRunView {
        let input = parse_intent("version 2\nmission \"garden/work\" state=\"ready\" { goal \"Tend the garden\"; step \"current\" { assigned-to \"agent/garden/orchard\" }; step \"queued\" { assigned-to \"agent/garden/orchard\" }; }","amber").unwrap();
        let preview = self
            .store
            .mission(
                &input,
                crate::model::IntentInput {
                    kdl: "".into(),
                    source_name: None,
                },
            )
            .unwrap();
        self.store
            .apply_as(
                &input,
                &preview.subject_tokens,
                "mission",
                Some("person/operator"),
            )
            .unwrap();
        let run = self
            .store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "garden/work".into(),
                revision: None,
                workspace: self.root.path().display().to_string(),
                requester: Some("person/operator".into()),
                mode: Some("run".into()),
                inputs: Default::default(),
                idempotency_key: "run".into(),
            })
            .unwrap();
        for step in &run.steps {
            self.store
                .set_step_state(&step.subject, "ready", None)
                .unwrap();
        }
        run
    }
    fn desired(&self) -> DesiredSubject {
        self.store
            .desired_subjects_named(&[SUBJECT.into()])
            .unwrap()
            .remove(0)
    }
    fn step(&self) {
        let reconciler = Reconciler::new(
            self.store.clone(),
            self.runtime.clone(),
            "amber".into(),
            Arc::new(Notify::new()),
        );
        let observed = self.runtime.observation.lock().unwrap().clone();
        assert!(
            reconciler
                .reconcile_rollout(&self.desired(), observed.as_ref(), None)
                .unwrap()
        );
    }
    fn operation(&self) -> rollout::Operation {
        self.store.rollout(SUBJECT).unwrap().unwrap()
    }
    fn ack(&self) {
        rollout::phase(
            &self.store,
            SUBJECT,
            &self.operation(),
            "drain-ack",
            None,
            &[],
        )
        .unwrap();
    }
    fn reopen(&mut self) {
        self.store = Arc::new(Store::open(&self.root.path().join("graph.db"), "amber").unwrap());
    }
}

#[test]
fn rollout_drains_then_resumes_once_across_disk_reopens() {
    let mut seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(30 * 60 * 1000, false)),
        false,
    );
    seat.step();
    assert_eq!(seat.operation().phase, "draining");
    assert!(rollout::hold_render(&seat.store, &seat.desired()).unwrap());
    seat.ack();
    seat.step();
    assert!(seat.runtime.stops.lock().unwrap().is_empty());
    seat.busy(false);
    seat.reopen();
    seat.step();
    assert_eq!(seat.operation().phase, "stopping");
    assert_eq!(
        seat.operation().native_session_id.as_deref(),
        Some("native-one")
    );
    seat.reopen();
    seat.step();
    assert_eq!(*seat.runtime.stops.lock().unwrap(), vec!["original-1"]);
    seat.reopen();
    seat.step();
    assert_eq!(seat.operation().phase, "starting");
    seat.reopen();
    seat.step();
    assert_eq!(seat.runtime.starts.lock().unwrap().len(), 1);
    let launch = seat.runtime.starts.lock().unwrap()[0].clone();
    assert_eq!(
        launch.environment[crate::suspension::RESUME_ENV],
        "native-one"
    );
    assert!(
        !launch
            .environment
            .contains_key(crate::suspension::CONTINUE_ENV)
    );
    seat.reopen();
    seat.step();
    assert_eq!(
        seat.operation().phase,
        "verifying",
        "{:?}",
        seat.operation().reason
    );
    seat.reopen();
    seat.step();
    assert_eq!(
        seat.operation().phase,
        "verifying",
        "{:?}",
        seat.operation().reason
    );
    seat.binding("replacement-1", "native-one");
    seat.step();
    assert_eq!(seat.operation().phase, "running");
    assert!(!seat.operation().holds_intake());
    assert_eq!(seat.runtime.starts.lock().unwrap().len(), 1);
}

#[test]
fn rollout_wrong_incarnation_never_terminates_its_successor() {
    let seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    seat.step();
    seat.runtime
        .observation
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .incarnation_id = Some("someone-else".into());
    seat.step();
    assert_eq!(seat.operation().phase, "blocked");
    assert!(seat.runtime.stops.lock().unwrap().is_empty());
    seat.step();
    assert!(seat.runtime.stops.lock().unwrap().is_empty());
}

#[test]
fn rollout_default_deadline_holds_and_force_only_overrides_busy_work() {
    for force in [false, true] {
        let seat = Seat::new();
        seat.publish(2, "second", Some(Policy::when_idle(1, force)), false);
        seat.step();
        seat.ack();
        std::thread::sleep(std::time::Duration::from_millis(3));
        seat.step();
        assert_eq!(
            seat.operation().phase,
            if force { "stopping" } else { "held" }
        );
        assert_eq!(seat.operation().forced, force);
        if !force {
            assert!(!seat.operation().holds_intake());
            seat.step();
            assert!(seat.runtime.stops.lock().unwrap().is_empty());
        }
    }
}

#[test]
fn rollout_session_mismatch_stops_only_failed_replacement_and_never_falls_back() {
    let seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    for _ in 0..5 {
        seat.step();
    }
    assert_eq!(
        seat.operation().phase,
        "verifying",
        "{:?}",
        seat.operation().reason
    );
    seat.binding("replacement-1", "wrong-session");
    seat.step();
    assert_eq!(seat.operation().phase, "failed");
    assert_eq!(
        *seat.runtime.stops.lock().unwrap(),
        vec!["original-1", "replacement-1"]
    );
    for _ in 0..3 {
        seat.step();
    }
    assert_eq!(seat.runtime.starts.lock().unwrap().len(), 1);
}

#[test]
fn rollout_omission_drains_before_retiring_without_a_replacement() {
    let seat = Seat::new();
    seat.publish(2, "", Some(Policy::when_idle(1_800_000, false)), true);
    seat.step();
    seat.ack();
    seat.step();
    assert_eq!(seat.operation().phase, "draining");
    seat.busy(false);
    seat.step();
    seat.step();
    seat.step();
    assert_eq!(seat.operation().phase, "retired");
    assert!(seat.runtime.starts.lock().unwrap().is_empty());
}

#[test]
fn rollout_holds_independent_mail_but_preserves_responses_and_staging_fences() {
    let seat = Seat::new();
    let mail = |id: &str, from: &str, to: &str, parent: Option<&str>| {
        seat.store.append_claim(&ClaimInput {subject:format!("message/{id}"),kind:"message.sent".into(),actor:Some(from.into()),
            fields:serde_json::from_value(json!({"status":"sent","from":from,"to":to,"content":"test","in_reply_to":parent})).unwrap(),
            evidence:Vec::new(),expected_subject:None,idempotency_key:None}).unwrap();
        seat.store
            .message(&format!("message/{id}"))
            .unwrap()
            .unwrap()
    };
    let outgoing = mail("question", SUBJECT, "person/operator", None);
    let binding = seat
        .store
        .bind_mailbox(&crate::mailbox::Fence::new(
            SUBJECT,
            "original-1",
            "delivery",
        ))
        .unwrap();
    let independent = mail("independent", "person/operator", SUBJECT, None);
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    let response = mail(
        "response",
        "person/operator",
        SUBJECT,
        Some(&outgoing.subject),
    );
    assert!(!seat.store.rollout_message_allowed(&independent).unwrap());
    assert!(seat.store.rollout_message_allowed(&response).unwrap());
    let stage = |message: &str| {
        seat.store.append_mailbox_receipt(
            &ClaimInput {
                subject: message.into(),
                kind: "message.staged".into(),
                actor: Some(SUBJECT.into()),
                fields: serde_json::from_value(json!({"status":"staged","recipient":SUBJECT}))
                    .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            },
            &binding,
        )
    };
    assert_eq!(
        stage(&independent.subject).unwrap_err().code,
        "seat-rollout-draining"
    );
    assert!(stage(&response.subject).is_ok());
    assert_eq!(
        seat.store
            .message(&independent.subject)
            .unwrap()
            .unwrap()
            .status,
        "sent"
    );
    let operation = seat.operation();
    rollout::phase(&seat.store, SUBJECT, &operation, "held", None, &[]).unwrap();
    assert!(seat.store.rollout_message_allowed(&independent).unwrap());
    assert!(stage(&independent.subject).is_ok());
}

#[test]
fn rollout_new_work_is_held_while_current_work_renews_and_completes() {
    let seat = Seat::new();
    let run = seat.work();
    let request = |key: &str| crate::model::WorkRequest {
        actor: Some(SUBJECT.into()),
        incarnation: Some("original-1".into()),
        summary: Some("tending".into()),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    seat.store
        .work_action(&run.steps[0].subject, "claim", &request("current"))
        .unwrap();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    let error = seat
        .store
        .work_action(&run.steps[1].subject, "claim", &request("queued"))
        .unwrap_err();
    assert_eq!(error.code, "seat-rollout-draining");
    seat.store
        .work_action(&run.steps[0].subject, "renew", &request("renew"))
        .unwrap();
    seat.store
        .work_action(&run.steps[0].subject, "progress", &request("progress"))
        .unwrap();
    seat.step();
    assert_eq!(seat.operation().phase, "draining");
    seat.store
        .work_action(&run.steps[0].subject, "complete", &request("complete"))
        .unwrap();
    seat.store
        .set_step_state(&run.steps[0].subject, "completed", None)
        .unwrap();
    seat.step();
    assert_eq!(seat.operation().phase, "stopping");
}

#[test]
fn rollout_unchanged_newer_receipts_keep_the_deadline_and_labels_do_not_restart() {
    let seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    let original = seat.operation();
    seat.publish(
        3,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    let same = seat.operation();
    assert_eq!(same.id, original.id);
    assert_eq!(same.deadline_unix_ms, original.deadline_unix_ms);
    seat.publish(4, "third", Some(Policy::when_idle(1_800_000, false)), false);
    assert_eq!(seat.operation().phase, "superseded");
    seat.step();
    assert_ne!(seat.operation().id, original.id);
    assert!(seat.runtime.stops.lock().unwrap().is_empty());
}

#[test]
fn rollout_recovers_a_spawn_receipt_gap_from_the_exact_driver_marker() {
    let mut seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    seat.step();
    seat.step();
    seat.step();
    let operation = seat.operation();
    assert_eq!(operation.phase, "starting");
    // Durable launch intent exists, but model a daemon crash before it records start success.
    rollout::phase(
        &seat.store,
        SUBJECT,
        &operation,
        "start-attempted",
        None,
        &[],
    )
    .unwrap();
    let member = seat.desired().member.unwrap();
    seat.runtime.start(&member).unwrap();
    seat.append("harness.observed",json!({"state":"idle","driver":"claude","incarnation_id":"replacement-1","quiescent":true,"rollout_operation":operation.id}));
    seat.binding("replacement-1", "native-one");
    seat.reopen();
    seat.step();
    assert_eq!(
        seat.operation().phase,
        "verifying",
        "{:?}",
        seat.operation().reason
    );
    seat.reopen();
    seat.step();
    assert_eq!(seat.operation().phase, "running");
    assert_eq!(seat.runtime.starts.lock().unwrap().len(), 1);
}

#[test]
fn rollout_supersession_after_positive_exit_starts_only_the_new_winner() {
    let mut seat = Seat::new();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    seat.step();
    seat.step();
    seat.step();
    let previous = seat.operation();
    assert_eq!(previous.phase, "starting");
    seat.publish(3, "third", Some(Policy::when_idle(1_800_000, false)), false);
    seat.reopen();
    seat.step();
    let current = seat.operation();
    assert_ne!(current.id, previous.id);
    assert_eq!(current.source.sequence, 3);
    assert_eq!(current.native_session_id.as_deref(), Some("native-one"));
    seat.step();
    seat.step();
    seat.binding("replacement-1", "native-one");
    seat.step();
    assert_eq!(seat.operation().phase, "running");
    let launches = seat.runtime.starts.lock().unwrap();
    assert_eq!(launches.len(), 1);
    assert!(format!("{:?}", launches[0].launch).contains("third"));
}

#[test]
fn rollout_retry_after_failed_verification_keeps_the_original_conversation() {
    let mut seat = Seat::new();
    let policy = Policy::when_idle(1_800_000, false);
    seat.publish(2, "second", Some(policy.clone()), false);
    seat.step();
    seat.ack();
    seat.busy(false);
    for _ in 0..5 {
        seat.step();
    }
    seat.binding("replacement-1", "wrong-session");
    seat.step();
    seat.step(); // Persist positive exit of the failed replacement before retry.
    seat.reopen();
    let failed = seat.operation();
    let actual = seat.store.latest_actual_value(SUBJECT).unwrap().unwrap();
    assert_eq!(actual["status"], "stopped");
    assert_eq!(actual["incarnation_id"], "replacement-1");
    let desired = seat.desired();
    let token = seat.store.selected_desired_token(SUBJECT).unwrap().unwrap();
    seat.store
        .request_rollout(
            SUBJECT,
            &token,
            desired.member.as_ref().unwrap(),
            "replacement-1",
            "person/operator",
            &policy,
            "retry-failed",
        )
        .unwrap();
    assert_ne!(seat.operation().id, failed.id);
    assert_eq!(
        seat.operation().native_session_id.as_deref(),
        Some("native-one")
    );
    for _ in 0..3 {
        seat.step();
    }
    assert_eq!(
        seat.operation().phase,
        "verifying",
        "{:?}",
        seat.operation().reason
    );
    assert_eq!(
        seat.runtime.starts.lock().unwrap()[1].environment[crate::suspension::RESUME_ENV],
        "native-one"
    );
    seat.binding("replacement-2", "native-one");
    seat.step();
    assert_eq!(seat.operation().phase, "running");
}

#[test]
fn rollout_newer_identical_receipt_before_spawn_verifies_the_captured_launch_token() {
    let seat = Seat::new();
    let policy = Policy::when_idle(1_800_000, false);
    seat.publish(2, "second", Some(policy.clone()), false);
    seat.step();
    let request = seat.operation();
    seat.ack();
    seat.busy(false);
    for _ in 0..3 {
        seat.step();
    }
    assert_eq!(seat.operation().phase, "starting");
    seat.publish(3, "second", Some(policy), false);
    for _ in 0..2 {
        seat.step();
    }
    assert_eq!(seat.operation().id, request.id);
    seat.binding("replacement-1", "native-one");
    seat.step();
    assert_eq!(seat.operation().phase, "running");
    seat.publish(4, "second", Some(Policy::when_idle(5_000, true)), false);
    assert!(seat.store.rollout(SUBJECT).unwrap().is_none());
    let reconciler = Reconciler::new(
        seat.store.clone(),
        seat.runtime.clone(),
        "amber".into(),
        Arc::new(Notify::new()),
    );
    assert!(
        !reconciler
            .reconcile_rollout(
                &seat.desired(),
                seat.runtime.observation.lock().unwrap().as_ref(),
                None
            )
            .unwrap()
    );
    assert_eq!(seat.runtime.starts.lock().unwrap().len(), 1);
}

#[test]
fn rollout_rechecks_pending_replies_and_the_physical_render_fence() {
    let seat = Seat::new();
    seat.store
        .append_claim(&ClaimInput {
            subject: "message/question-at-boundary".into(),
            kind: "message.sent".into(),
            actor: Some(SUBJECT.into()),
            fields: serde_json::from_value(
                json!({"from":SUBJECT,"to":"person/operator","content":"question","status":"sent"}),
            )
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    seat.step();
    assert_eq!(seat.operation().phase, "stopping");
    seat.store.append_claim(&ClaimInput { subject:"message/answer-at-boundary".into(),kind:"message.sent".into(),actor:Some("person/operator".into()),
        fields:serde_json::from_value(json!({"from":"person/operator","to":SUBJECT,"content":"answer","status":"sent","in_reply_to":"message/question-at-boundary"})).unwrap(),
        evidence:Vec::new(),expected_subject:None,idempotency_key:None }).unwrap();
    seat.step();
    assert!(seat.runtime.stops.lock().unwrap().is_empty());
    let reconciler = Reconciler::new(
        seat.store.clone(),
        seat.runtime.clone(),
        "amber".into(),
        Arc::new(Notify::new()),
    );
    let mut operation = seat.operation();
    operation.replacement_incarnation = Some("replacement-1".into());
    rollout::phase(&seat.store, SUBJECT, &operation, "verifying", None, &[]).unwrap();
    assert!(reconciler.rollout_render_guard(&seat.desired()).is_err());
}

#[test]
fn rollout_pending_person_work_can_resume_and_finish_while_new_work_waits() {
    for (retire, answered_before_drain) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let seat = Seat::new();
        let run = seat.work();
        let request = |key: &str| crate::model::WorkRequest {
            actor: Some(SUBJECT.into()),
            incarnation: Some("original-1".into()),
            summary: Some("tending".into()),
            reason: None,
            evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        seat.store
            .work_action(&run.steps[0].subject, "claim", &request("claim"))
            .unwrap();
        let ask = seat
            .store
            .ask_person(&crate::model::PersonAskRequest {
                legacy_request: None,
                person: "person/operator".into(),
                title: "Which bed?".into(),
                reason: "Choose the next bed".into(),
                actor: SUBJECT.into(),
                step: Some(run.steps[0].subject.clone()),
                new_run: None,
                incarnation: Some("original-1".into()),
                request: None,
                idempotency_key: "bed".into(),
            })
            .unwrap();
        seat.publish(
            2,
            if retire { "" } else { "second" },
            Some(Policy::when_idle(1_800_000, false)),
            retire,
        );
        let answer = || {
            seat.store
                .finish_person_step(
                    &crate::model::PersonStepResponse {
                        subject: ask.subject.clone(),
                        actor: "person/operator".into(),
                        summary: "The north bed".into(),
                        evidence: Vec::new(),
                        episode: None,
                        answer: None,
                        idempotency_key: "north".into(),
                    },
                    false,
                )
                .unwrap();
        };
        if answered_before_drain {
            answer();
        }
        seat.store.reconcile_person_asks().unwrap();
        seat.step();
        seat.store.reconcile_person_asks().unwrap();
        seat.ack();
        seat.busy(false);
        seat.step();
        assert_eq!(seat.operation().phase, "draining");
        assert!(
            seat.operation()
                .blocking
                .iter()
                .any(|b| b == "pending-person-work")
        );
        if !answered_before_drain {
            answer();
        }
        seat.step();
        assert_eq!(seat.operation().phase, "draining");
        seat.store
            .work_action(&run.steps[0].subject, "claim", &request("resume"))
            .unwrap();
        assert_eq!(
            seat.store
                .work_action(&run.steps[1].subject, "claim", &request("unrelated"))
                .unwrap_err()
                .code,
            "seat-rollout-draining"
        );
        seat.store
            .work_action(&run.steps[0].subject, "complete", &request("done"))
            .unwrap();
        seat.store
            .set_step_state(&run.steps[0].subject, "completed", None)
            .unwrap();
        seat.step();
        assert_eq!(seat.operation().phase, "stopping");
    }
}

#[test]
fn rollout_refuses_to_move_the_original_conversation_between_login_accounts() {
    let seat = Seat::new();
    seat.append("harness.session-file",json!({"harness":"claude","session_id":"native-one","incarnation_id":"original-1","account_ref":"cloud"}));
    seat.publish(
        2,
        "second",
        Some(Policy::when_idle(1_800_000, false)),
        false,
    );
    seat.step();
    seat.ack();
    seat.busy(false);
    for _ in 0..4 {
        seat.step();
    }
    assert_eq!(seat.operation().phase, "failed");
    assert!(
        seat.operation()
            .reason
            .unwrap()
            .contains("native login accounts")
    );
    assert_eq!(seat.operation().native_account.as_deref(), Some("cloud"));
    assert!(seat.runtime.starts.lock().unwrap().is_empty());
    assert_eq!(*seat.runtime.stops.lock().unwrap(), vec!["original-1"]);
}
