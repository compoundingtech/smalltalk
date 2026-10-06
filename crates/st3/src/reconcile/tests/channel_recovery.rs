use super::*;
use serde_json::json;

const SEAT: &str = "agent/node.orchid";

fn setup() -> (
    Arc<Store>,
    Arc<FakeRuntime>,
    Reconciler<FakeRuntime>,
    DesiredSubject,
) {
    let store = Arc::new(Store::open_memory("node").unwrap());
    apply_source(
        &store,
        "version 2\nagent \"orchid\" { workspace \"/tmp\"; restart \"never\"; harness \"claude\" {} }",
        "channel-recovery",
    );
    let desired = store.desired_subjects().unwrap().remove(0);
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    (store, runtime, reconciler, desired)
}

fn attachment(store: &Store, incarnation: &str, attached: bool) -> u128 {
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: Some(SEAT.into()),
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("runtime_id".into(), json!("node.orchid")),
                ("incarnation_id".into(), json!(incarnation)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "harness.observed".into(),
            actor: Some(SEAT.into()),
            fields: BTreeMap::from([
                ("state".into(), json!("idle")),
                ("driver".into(), json!("claude")),
                ("incarnation_id".into(), json!(incarnation)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "harness.diagnostic".into(),
            actor: Some(SEAT.into()),
            fields: BTreeMap::from([
                (
                    "code".into(),
                    json!(if attached {
                        "claude-channel-attached"
                    } else {
                        "claude-channel-unattached"
                    }),
                ),
                (
                    "status".into(),
                    json!(if attached { "recovered" } else { "blocked" }),
                ),
                ("incarnation_id".into(), json!(incarnation)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
        .accepted_at_unix_ms
}

fn recover(
    reconciler: &Reconciler<FakeRuntime>,
    desired: &DesiredSubject,
    incarnation: &str,
    at: u128,
) -> Result<bool> {
    reconciler.reconcile_claude_channel_recovery(
        desired,
        desired.member.as_ref().unwrap(),
        Some(&claude_seat_pty("orchid", "running", incarnation)),
        None,
        at,
    )
}

fn attempts(store: &Store) -> Vec<crate::model::ClaimRecord> {
    store
        .claims_for(SEAT, Some("runtime.action.requested"))
        .unwrap()
        .into_iter()
        .filter(|claim| claim.body["fields"]["operation"] == "claude-channel-recovery")
        .collect()
}

#[test]
fn channel_recovery_rechecks_then_restarts_once_and_ignores_a_stale_incarnation() {
    let (store, runtime, reconciler, desired) = setup();
    let since = attachment(&store, "orchid-one", false);
    let (result, deadline) =
        smallclaims::touched::record_due(|| recover(&reconciler, &desired, "orchid-one", since));
    assert!(!result.unwrap());
    assert_eq!(deadline, Some(since + 10_000));
    assert!(attempts(&store).is_empty());
    assert!(!recover(&reconciler, &desired, "predecessor", since + 10_000).unwrap());
    assert!(recover(&reconciler, &desired, "orchid-one", since + 10_000).unwrap());
    assert!(!recover(&reconciler, &desired, "orchid-one", since + 20_000).unwrap());
    assert_eq!(attempts(&store).len(), 1);
    let request = &attempts(&store)[0];
    assert!(
        request.body["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("attempt 1 of 3")
    );
    assert_eq!(
        request.body["evidence"][0],
        store.selected_desired_token(SEAT).unwrap().unwrap()
    );
    // The regular restart path stops this exact seat even under restart "never".
    assert!(
        reconciler
            .reconcile_requested_restart(
                &desired,
                desired.member.as_ref().unwrap(),
                Some(&claude_seat_pty("orchid", "running", "orchid-one")),
                None
            )
            .unwrap()
    );
    assert_eq!(*runtime.stops.lock().unwrap(), ["node.orchid"]);
    let restarted = Reconciler::new(
        store,
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    assert!(!recover(&restarted, &desired, "orchid-one", since + 30_000).unwrap());
}

#[test]
fn channel_initialized_during_recheck_keeps_its_incarnation() {
    let (store, runtime, reconciler, desired) = setup();
    let since = attachment(&store, "orchid-one", false);
    assert!(!recover(&reconciler, &desired, "orchid-one", since).unwrap());
    attachment(&store, "orchid-one", true);
    assert!(!recover(&reconciler, &desired, "orchid-one", since + 30_000).unwrap());
    assert!(attempts(&store).is_empty());
    assert!(runtime.stops.lock().unwrap().is_empty());
}

#[test]
fn channel_initialization_can_cancel_a_recorded_recovery_before_the_stop() {
    let (store, runtime, reconciler, desired) = setup();
    let since = attachment(&store, "orchid-one", false);
    assert!(recover(&reconciler, &desired, "orchid-one", since + 10_000).unwrap());
    attachment(&store, "orchid-one", true);
    assert!(!recover(&reconciler, &desired, "orchid-one", since + 10_001).unwrap());
    assert!(
        !reconciler
            .reconcile_requested_restart(
                &desired,
                desired.member.as_ref().unwrap(),
                Some(&claude_seat_pty("orchid", "running", "orchid-one")),
                None
            )
            .unwrap()
    );
    assert!(runtime.stops.lock().unwrap().is_empty());
    assert_eq!(attempts(&store).len(), 1);
}

#[test]
fn recovery_success_requires_channel_attachment_even_when_hooks_are_ready() {
    let (store, _, reconciler, desired) = setup();
    let since = attachment(&store, "orchid-one", false);
    recover(&reconciler, &desired, "orchid-one", since + 10_000).unwrap();
    let request = attempts(&store).pop().unwrap();
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.action.requested".into(),
            actor: request.actor.clone(),
            fields: BTreeMap::from([("action".into(), json!("start"))]),
            evidence: vec![request.id.clone()],
            expected_subject: None,
            idempotency_key: Some(format!("agent-restart-attempt:{}", request.id)),
        })
        .unwrap();
    for (kind, fields) in [
        (
            "runtime.observed",
            BTreeMap::from([
                ("status".into(), json!("running")),
                ("runtime_id".into(), json!("node.orchid")),
                ("incarnation_id".into(), json!("orchid-two")),
            ]),
        ),
        (
            "harness.observed",
            BTreeMap::from([
                ("state".into(), json!("idle")),
                ("driver".into(), json!("claude")),
                ("incarnation_id".into(), json!("orchid-two")),
            ]),
        ),
    ] {
        store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: kind.into(),
                actor: Some(SEAT.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let replacement = claude_seat_pty("orchid", "running", "orchid-two");
    assert!(store.current_harness(SEAT).unwrap().unwrap().is_ready());
    assert!(
        !reconciler
            .reconcile_requested_restart(
                &desired,
                desired.member.as_ref().unwrap(),
                Some(&replacement),
                None
            )
            .unwrap()
    );
    let completion = format!("agent-restart-completed:{}", request.id);
    assert!(store.operation_claim(&completion).unwrap().is_none());
    attachment(&store, "orchid-two", true);
    reconciler
        .reconcile_requested_restart(
            &desired,
            desired.member.as_ref().unwrap(),
            Some(&replacement),
            None,
        )
        .unwrap();
    assert_eq!(
        store.operation_claim(&completion).unwrap().unwrap().kind,
        "runtime.action.succeeded"
    );
}

#[test]
fn channel_recovery_budget_and_parking_survive_daemon_restarts() {
    let (store, runtime, reconciler, desired) = setup();
    for (index, backoff) in [10_000, 20_000, 40_000].into_iter().enumerate() {
        let incarnation = format!("orchid-{index}");
        let since = attachment(&store, &incarnation, false);
        let fresh = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        assert!(!recover(&fresh, &desired, &incarnation, since + backoff - 1).unwrap());
        assert!(recover(&fresh, &desired, &incarnation, since + backoff).unwrap());
        assert_eq!(attempts(&store).len(), index + 1);
    }
    let since = attachment(&store, "orchid-last", false);
    assert!(recover(&reconciler, &desired, "orchid-last", since).unwrap());
    let fresh = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    let failure = recover(&fresh, &desired, "orchid-last", since + 1).unwrap_err();
    assert!(failure.to_string().contains("parked"));
    assert!(failure.to_string().contains("mail remains held"));
    assert_eq!(*runtime.stops.lock().unwrap(), ["node.orchid"]);
    assert_eq!(attempts(&store).len(), 3);
    assert!(
        fresh
            .reconcile_claude_channel_recovery(
                &desired,
                desired.member.as_ref().unwrap(),
                None,
                None,
                since + 100_000
            )
            .is_err()
    );
    assert!(runtime.starts.lock().unwrap().is_empty());

    // An explicit restart reopens a budget without discarding the durable failure evidence.
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.action.requested".into(),
            actor: Some("person/eval".into()),
            fields: BTreeMap::from([
                ("action".into(), json!("restart")),
                ("incarnation_id".into(), json!("orchid-last")),
            ]),
            evidence: vec![store.selected_desired_token(SEAT).unwrap().unwrap()],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let since = attachment(&store, "orchid-explicit", false);
    assert!(recover(&fresh, &desired, "orchid-explicit", since + 10_000).unwrap());
    assert!(
        attempts(&store).last().unwrap().body["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("attempt 1 of 3")
    );
}

#[test]
fn a_verified_recovery_resets_the_budget_for_a_later_channel_loss() {
    let (store, runtime, reconciler, desired) = setup();
    let since = attachment(&store, "orchid-one", false);
    recover(&reconciler, &desired, "orchid-one", since + 10_000).unwrap();
    let request = attempts(&store).pop().unwrap();
    attachment(&store, "orchid-two", true);
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.action.succeeded".into(),
            actor: request.actor,
            fields: BTreeMap::from([("action".into(), json!("restart"))]),
            evidence: vec![request.id],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let since = attachment(&store, "orchid-two", false);
    let fresh = Reconciler::new(
        store.clone(),
        runtime,
        "node".into(),
        Arc::new(Notify::new()),
    );
    assert!(recover(&fresh, &desired, "orchid-two", since + 10_000).unwrap());
    assert!(
        attempts(&store).last().unwrap().body["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("attempt 1 of 3")
    );
}
