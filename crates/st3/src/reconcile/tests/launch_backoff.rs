use super::*;
pub(super) struct Clock;
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}
pub(super) fn at(store: &Store, now: u128) {
    smallclaims::store::set_thread_clock(Some(now));
    store.set_write_clock_at(now).unwrap();
}
fn fixture(now: u128) -> (Arc<Store>, Reconciler<FakeRuntime>, DesiredSubject) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    at(&store, now);
    apply_source(
        &store,
        "version 2\nagent \"example/worker\" { workspace \"/tmp\"; command \"true\"; restart always; }",
        "launch-backoff",
    );
    let desired = store
        .desired_subject_with_writer("agent/example/worker")
        .unwrap()
        .unwrap()
        .0;
    let reconciler = Reconciler::new(
        store.clone(),
        Arc::new(FakeRuntime::default()),
        "node".into(),
        Arc::new(Notify::new()),
    );
    (store, reconciler, desired)
}
fn launch(
    store: &Store,
    reconciler: &Reconciler<FakeRuntime>,
    desired: &DesiredSubject,
    incarnation: &str,
) {
    store
        .append_claim(&ClaimInput {
            subject: desired.subject.clone(),
            kind: "runtime.action.succeeded".into(),
            actor: Some(desired.subject.clone()),
            fields: BTreeMap::from([
                ("action".into(), Value::String("start".into())),
                (
                    "desired_token".into(),
                    Value::String(reconciler.launch_token(&desired.subject).unwrap()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}
#[test]
fn crash_backoff_caps_total_delay_and_keeps_absolute_due_across_accounting_windows() {
    let _clock = Clock;
    let mut now = 1_800_000_000_000;
    let (store, reconciler, desired) = fixture(now);
    let mut member = desired.member.clone().unwrap();
    member.restart_intensity.attempts = 1000;
    for attempt in 1..=8 {
        let incarnation = format!("fake-{attempt}");
        launch(&store, &reconciler, &desired, &incarnation);
        now += 100;
        at(&store, now);
        let observed = RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(1),
            incarnation_id: Some(incarnation),
        };
        reconciler
            .record_member(&desired, &observed, false)
            .unwrap();
        let RestartDecision::Wait { until, .. } = reconciler
            .restart_decision(&desired, &member, &observed)
            .unwrap()
        else {
            panic!(
                "failure {attempt} did not back off: now={} subject={} lifecycle={:?} launches={:?} exit={:?}",
                now_ms(),
                desired.kind,
                member.lifecycle,
                store
                    .observations_for(&desired.subject, "runtime.action.succeeded")
                    .unwrap(),
                store
                    .latest_observation(&desired.subject, "runtime.observed")
                    .unwrap()
            )
        };
        let minimum = (5000u128 << (attempt - 1).min(6)).min(120000);
        assert!(
            (minimum..=minimum.saturating_add(1000).min(120000))
                .contains(&until.saturating_sub(now))
        );
        let index = store.index().unwrap();
        assert!(
            matches!(reconciler.restart_decision(&desired,&member,&observed).unwrap(),RestartDecision::Wait {until:again,..} if again==until)
        );
        assert_eq!(
            store.index().unwrap(),
            index,
            "unchanged failure rewrote history"
        );
        at(&store, until - 1);
        assert!(
            matches!(reconciler.restart_decision(&desired,&member,&observed).unwrap(),RestartDecision::Wait {until:again,..} if again==until)
        );
        at(&store, until);
        assert!(matches!(
            reconciler
                .restart_decision(&desired, &member, &observed)
                .unwrap(),
            RestartDecision::Start
        ));
        now = until;
    }
}
#[test]
fn healthy_lifetime_resets_consecutive_crash_delay() {
    let _clock = Clock;
    let now = 1_800_000_000_000;
    let (store, reconciler, desired) = fixture(now);
    let member = desired.member.as_ref().unwrap();
    let token = reconciler.launch_token(&desired.subject).unwrap();
    reconciler
        .record_once(
            &desired.subject,
            "runtime.reconcile-decision",
            BTreeMap::from([
                ("decision".into(), Value::String("wait".into())),
                (
                    "key".into(),
                    Value::String(format!("crash-backoff:{token}:old:6")),
                ),
            ]),
        )
        .unwrap();
    launch(&store, &reconciler, &desired, "healthy");
    at(&store, now + 60001);
    let mut observed = RuntimeObservation {
        runtime_id: member.runtime_id.clone(),
        terminal: true,
        status: "exited".into(),
        exit_code: Some(1),
        incarnation_id: Some("healthy".into()),
    };
    reconciler
        .record_member(&desired, &observed, false)
        .unwrap();
    assert!(matches!(
        reconciler
            .restart_decision(&desired, member, &observed)
            .unwrap(),
        RestartDecision::Start
    ));
    launch(&store, &reconciler, &desired, "fresh");
    at(&store, now + 60101);
    observed.incarnation_id = Some("fresh".into());
    reconciler
        .record_member(&desired, &observed, false)
        .unwrap();
    assert!(
        matches!(reconciler.restart_decision(&desired,member,&observed).unwrap(),RestartDecision::Wait {until,..} if until-(now+60101)<=6000)
    );
}

#[test]
fn automatic_crash_delay_preserves_a_longer_declared_restart_delay() {
    let _clock = Clock;
    let now = 1_800_000_000_000;
    let (store, reconciler, desired) = fixture(now);
    let mut member = desired.member.clone().unwrap();
    member.restart_intensity.delay_ms = 20_000;
    launch(&store, &reconciler, &desired, "delayed");
    at(&store, now + 100);
    let observation = RuntimeObservation {
        runtime_id: member.runtime_id.clone(),
        terminal: true,
        status: "exited".into(),
        exit_code: Some(1),
        incarnation_id: Some("delayed".into()),
    };
    reconciler
        .record_member(&desired, &observation, false)
        .unwrap();
    assert!(
        matches!(reconciler.restart_decision(&desired, &member, &observation).unwrap(), RestartDecision::Wait {until,..} if until == now + 20_100)
    );
}
