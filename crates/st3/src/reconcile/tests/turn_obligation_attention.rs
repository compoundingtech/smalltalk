use super::*;
use serde_json::json;
use st_drivers::turn_obligation::{Ledger, Obligation, Terminal};

#[test]
fn turn_obligation_owner_fault_is_once_per_live_episode_and_exact_terminal_resolves() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let workspace = tempfile::tempdir().unwrap();
    apply_source(
        &store,
        &format!(
            "version 2\nagent \"debt\" {{ workspace {:?}; harness \"codex\" {{}} }}",
            workspace.path().display().to_string()
        ),
        "owed-desired",
    );
    let desired = store.desired_subjects().unwrap().remove(0);
    let runtime = Arc::new(FakeRuntime::default());
    let reconciler = Reconciler::new(
        store.clone(),
        runtime.clone(),
        "node".into(),
        Arc::new(Notify::new()),
    );
    reconciler.reconcile_once().unwrap();
    let observed = |incarnation: &str| RuntimeObservation {
        runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
        terminal: true,
        status: "running".into(),
        exit_code: None,
        incarnation_id: Some(incarnation.into()),
    };
    *runtime.ptys.lock().unwrap() = vec![observed("old")];
    reconciler.reconcile_once().unwrap();
    let owed = Obligation {
        source_sequence: 1,
        provider_incarnation: "provider-old".into(),
        ownership_sequence: 1,
        runtime_incarnation: Some("old".into()),
        desired_revision: Some(crate::store::desired_revision(&desired)),
        native_session_id: Some("session".into()),
        native_turn_id: Some("native-turn".into()),
        started_at_ms: 1,
        pending_human: false,
        tool_outcome_unknown: false,
        pending_tool_ids: vec![],
    };
    let publish = |incarnation: &str, state: &str, ledger: Option<Ledger>| {
        let mut fields = BTreeMap::from([
            ("state".into(), json!(state)),
            ("incarnation_id".into(), json!(incarnation)),
            (
                "evidence_incarnation".into(),
                json!(format!("provider-{incarnation}")),
            ),
            ("ownership_sequence".into(), json!(1)),
            ("driver".into(), json!("codex")),
        ]);
        if let Some(ledger) = ledger {
            fields.insert(
                "turn_obligation".into(),
                serde_json::to_value(ledger).unwrap(),
            );
        }
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields,
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    };
    publish(
        "old",
        "working",
        Some(Ledger {
            sequence: 1,
            open: vec![owed.clone()],
            ..Ledger::default()
        }),
    );
    reconciler
        .reconcile_turn_obligation(&desired, &observed("old"))
        .unwrap();
    assert_eq!(
        store
            .current_harness(&desired.subject)
            .unwrap()
            .unwrap()
            .turn_recovery
            .unwrap()["state"],
        "in-flight"
    );
    assert!(store.fault_items(None).unwrap().is_empty());

    *runtime.ptys.lock().unwrap() = vec![observed("new")];
    reconciler.reconcile_once().unwrap();
    let stale_notice = AttentionRequest {
        reviewer: "person/operator".into(),
        title: "Fixture stale fault".into(),
        reason: "Fixture old invocation".into(),
        severity: "error".into(),
        targets: vec![desired.subject.clone()],
        actor: RECONCILER_ACTOR.into(),
        idempotency_key: "fixture-stale-runtime".into(),
    };
    let selected = store
        .selected_desired_token(&desired.subject)
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .record_turn_obligation_failure(
                "attention/fixture-stale",
                &stale_notice,
                &desired,
                &selected,
                "old"
            )
            .unwrap_err()
            .code,
        "stale-harness-event-session"
    );
    publish("new", "idle", None);
    for _ in 0..3 {
        reconciler
            .reconcile_turn_obligation(&desired, &observed("new"))
            .unwrap();
    }
    let failures = store
        .claims_for(&desired.subject, Some("operational.failure"))
        .unwrap()
        .into_iter()
        .filter(|claim| claim.body["fields"]["condition"] == "turn-obligation")
        .collect::<Vec<_>>();
    assert_eq!(
        failures.len(),
        1,
        "quiet passes cannot resend the same owner episode"
    );
    assert_eq!(store.fault_items(Some("person/operator")).unwrap().len(), 1);
    assert_eq!(
        store
            .current_harness(&desired.subject)
            .unwrap()
            .unwrap()
            .state,
        "blocked"
    );
    let episode = failures[0].body["fields"]["episode"].as_str().unwrap();
    // Closing the notice quiets that episode; it does not assert native execution ended.
    store
        .recover_operational_failure(
            episode,
            "Fixture owner quieted the notice only",
            "fixture-dismiss-notice",
        )
        .unwrap();
    reconciler
        .reconcile_turn_obligation(&desired, &observed("new"))
        .unwrap();
    assert_eq!(
        store
            .claims_for(&desired.subject, Some("operational.failure"))
            .unwrap()
            .into_iter()
            .filter(|claim| claim.body["fields"]["condition"] == "turn-obligation")
            .count(),
        1,
        "a closed notice cannot refault the same captured obligation"
    );
    assert_eq!(
        store
            .current_harness(&desired.subject)
            .unwrap()
            .unwrap()
            .state,
        "blocked",
        "closing a notice is no execution proof"
    );
    let captured = store.turn_obligation_snapshot(&desired.subject).unwrap();
    let selected_receipt = &captured["evidence"]["selected_receipts"][0];
    store
        .acknowledge_turn_obligation(crate::store::turn_obligation::AcknowledgeRequest {
            subject: desired.subject.clone(),
            actor: desired.subject.clone(),
            receipts: vec![crate::store::turn_obligation::ReceiptEvidence {
                receipt: selected_receipt["receipt"].as_str().unwrap().into(),
                source_claim: selected_receipt["source_claim"].as_str().unwrap().into(),
            }],
            source_revision: captured["source_revision"].as_str().unwrap().into(),
            captured_cut: captured["captured_cut"].as_u64().unwrap(),
            reason: "Fixture inspected unknown execution; quiet action only".into(),
            idempotency_key: "fixture-owner-ack".into(),
        })
        .unwrap();
    for _ in 0..3 {
        publish("new", "idle", None);
        reconciler
            .reconcile_turn_obligation(&desired, &observed("new"))
            .unwrap();
        let harness = store.current_harness(&desired.subject).unwrap().unwrap();
        assert_eq!(harness.state, "idle");
        assert_eq!(harness.turn_recovery.as_ref().unwrap()["state"], "unknown");
        assert_eq!(
            harness.turn_recovery.unwrap()["owner_action_required"],
            false
        );
        assert!(store.fault_items(None).unwrap().is_empty());
    }
    assert_eq!(
        store
            .claims_for(&desired.subject, Some("operational.failure"))
            .unwrap()
            .into_iter()
            .filter(|claim| claim.body["fields"]["condition"] == "turn-obligation")
            .count(),
        1,
        "heartbeats and reconcile cannot wake an acknowledged interruption"
    );

    publish(
        "new",
        "idle",
        Some(Ledger {
            sequence: 1,
            terminal: vec![Terminal {
                obligation: owed,
                evidence_provider_incarnation: "provider-new".into(),
                outcome: "completed".into(),
                observed_at_ms: 2,
            }],
            ..Ledger::default()
        }),
    );
    reconciler
        .reconcile_turn_obligation(&desired, &observed("new"))
        .unwrap();
    assert_eq!(
        store.operational_failure(episode).unwrap().unwrap().status,
        "resolved"
    );
    assert!(store.fault_items(None).unwrap().is_empty());
    assert_eq!(
        store
            .current_harness(&desired.subject)
            .unwrap()
            .unwrap()
            .state,
        "idle"
    );
    assert!(
        runtime.keys.lock().unwrap().is_empty(),
        "recovery never sends native input"
    );

    apply_source(
        &store,
        &format!("version 2\nstop {:?}", desired.subject),
        "owed-stop",
    );
    reconciler.reconcile_once().unwrap();
    assert!(store.fault_items(None).unwrap().is_empty());
    assert_eq!(
        store
            .record_turn_obligation_failure(
                "attention/fixture-stale",
                &stale_notice,
                &desired,
                &selected,
                "new"
            )
            .unwrap_err()
            .code,
        "stale-observer-revision"
    );
}
