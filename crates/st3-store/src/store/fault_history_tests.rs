// The pre-sharing reader is an oracle for complete output and the extra history SELECT.
// It deliberately uses the independently folding wrappers, not the shared helper.
fn independently_folded_faults(store: &Store, as_of: u128) -> Result<Vec<FaultView>> {
    let mut items = store.subscription_fault_items(as_of)?;
    items.extend(store.operational_attention_items(None, as_of)?);
    items.extend(
        store.broken_gate_items(None, as_of)?
            .into_iter()
            .filter(|item| !item.person.starts_with("person/")),
    );
    items.extend(store.checkpoint_attention_items(None, as_of)?);
    let fallback = store.fleet_fault_agent()?;
    store.current_attention(items, as_of)?
        .into_iter()
        .map(|item| {
            Ok(FaultView {
                owner: store.fault_owner(&item, fallback.as_deref())?,
                item,
            })
        })
        .collect()
}

fn assert_fault_parity(store: &Store, as_of: u128) -> Vec<FaultView> {
    let expected = independently_folded_faults(store, as_of).unwrap();
    let actual = store.fault_snapshot(as_of).unwrap();
    assert_eq!(
        serde_json::to_value(&actual).unwrap(),
        serde_json::to_value(expected).unwrap(),
        "full items, owners and order at {as_of}",
    );
    actual
}

fn append_history_failure(
    store: &Store,
    episode: &str,
    title: &str,
    reviewer: &str,
    condition: &str,
    targets: Vec<String>,
) -> ClaimRecord {
    store.append_claim(&ClaimInput {
        subject: targets[0].clone(),
        kind: "operational.failure".into(),
        actor: Some("daemon/runtime".into()),
        fields: BTreeMap::from([
            ("episode".into(), json!(episode)),
            ("condition".into(), json!(condition)),
            ("reviewer".into(), json!(reviewer)),
            ("title".into(), json!(title)),
            ("reason".into(), json!(title)),
            ("severity".into(), json!("error")),
            ("targets".into(), json!(targets)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!("{episode}:{title}")),
    }).unwrap()
}

fn append_history_recovery(store: &Store, failure: &ClaimRecord, key: &str) {
    store.append_claim(&ClaimInput {
        subject: failure.subject.clone(),
        kind: "operational.recovered".into(),
        actor: Some("daemon/runtime".into()),
        fields: BTreeMap::from([
            ("episode".into(), failure.body["fields"]["episode"].clone()),
            ("failure".into(), json!(failure.id)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    }).unwrap();
}

#[test]
fn shared_fault_history_preserves_replacement_recovery_gate_owners_and_order() {
    let store = Store::open_memory("node").unwrap();
    let source = "version 2\nagent \"node.ops\" { command \"true\"; handles-faults }\n";
    let intent = parse_intent(source, "node").unwrap();
    let preview = store.mission(&intent, IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    store.apply(&intent, &preview.subject_tokens, "fault-history-owner").unwrap();
    let run = start_agentless_run(&store, "fault-history-gate");
    let targets = vec![
        run.subject.clone(), run.generation.clone(), "exec-result/test".into(),
        run.steps[0].subject.clone(),
    ];
    let first = append_history_failure(
        &store, "ordinary", "first", "agent/node.ops", "observed", vec!["daemon/node".into()],
    );
    append_history_failure(
        &store, "agent-gate", "agent gate", "agent/node.ops", GATE_BROKEN_CONDITION, targets.clone(),
    );
    append_history_failure(
        &store, "person-gate", "person gate", "person/alex", GATE_BROKEN_CONDITION, targets,
    );
    assert!(assert_fault_parity(&store, first.accepted_at_unix_ms - 1).is_empty());
    let pending = assert_fault_parity(&store, now_ms());
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|fault| fault.owner.as_deref() == Some("agent/node.ops")));
    assert!(pending.iter().any(|fault| fault.item.title == "first"));
    assert!(pending.iter().any(|fault| fault.item.title == "agent gate"));
    let person_items = store.attention_snapshot(Some("person/alex"), now_ms()).unwrap();
    assert_eq!(person_items.iter().filter(|item| item.title == "person gate").count(), 1);
    assert_eq!(store.broken_gate_items(Some("person/alex"), now_ms()).unwrap().len(), 1);

    let replaced = append_history_failure(
        &store, "ordinary", "replacement", "agent/node.ops", "observed", vec!["daemon/node".into()],
    );
    append_history_recovery(&store, &first, "stale-recovery");
    let stale = assert_fault_parity(&store, now_ms());
    assert_eq!(stale.len(), 2);
    assert!(stale.iter().any(|fault| fault.item.episode == replaced.id));
    assert!(stale.iter().all(|fault| fault.item.episode != first.id));
    append_history_recovery(&store, &replaced, "matching-recovery");
    // Recovery remains current even when evaluating an earlier explicit as_of.
    assert!(assert_fault_parity(&store, replaced.accepted_at_unix_ms)
        .iter().all(|fault| fault.item.subject != "daemon/node"));
    assert_eq!(assert_fault_parity(&store, now_ms()).len(), 1);
    store.set_mission_run_state(&run.id, "cancelled", "terminal", Some("gate ended")).unwrap();
    assert!(assert_fault_parity(&store, now_ms()).is_empty());
    assert!(store.broken_gate_items(None, now_ms()).unwrap().is_empty());
}

#[test]
fn fault_snapshot_removes_one_history_select_and_standalone_gate_still_folds() {
    let store = Store::open_memory("node").unwrap();
    append_history_failure(
        &store, "ordinary", "pending", "person/operator", "observed", vec!["daemon/node".into()],
    );
    let as_of = now_ms();
    // The scope counts connection-setup PRAGMAs too. Exercise both complete paths
    // outside the scopes so the first reader's nested pool growth is not charged
    // only to the independently folded path. This compares steady-state reads.
    independently_folded_faults(&store, as_of).unwrap();
    store.fault_snapshot(as_of).unwrap();
    let old_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let expected = independently_folded_faults(&store, as_of).unwrap();
    let old_work = old_scope.finish();
    let shared_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    let actual = store.fault_snapshot(as_of).unwrap();
    let shared_work = shared_scope.finish();
    assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    assert!(shared_work.statements > 0, "the SQL control must measure real work");
    assert_eq!(old_work.statements, shared_work.statements + 1);

    let (_, failures) = store.operational_attention_items_with_failures(None, as_of).unwrap();
    // Warm both gate paths before their separate scoped comparison as well.
    store.broken_gate_items_from_failures(None, as_of, failures.clone()).unwrap();
    store.broken_gate_items(None, as_of).unwrap();
    let shared_gate_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    store.broken_gate_items_from_failures(None, as_of, failures).unwrap();
    let shared_gate_work = shared_gate_scope.finish();
    let standalone_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
    store.broken_gate_items(None, as_of).unwrap();
    let standalone_work = standalone_scope.finish();
    assert_eq!(standalone_work.statements, shared_gate_work.statements + 1);
}

#[test]
fn later_fault_and_recovery_are_visible_on_the_next_call_and_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fault-history.sqlite3");
    let store = Store::open(&path, "node").unwrap();
    assert!(store.fault_snapshot(now_ms()).unwrap().is_empty());
    let first = append_history_failure(
        &store, "ordinary", "first", "person/operator", "observed", vec!["daemon/node".into()],
    );
    assert_eq!(assert_fault_parity(&store, now_ms()).len(), 1);
    append_history_recovery(&store, &first, "recover-before-reopen");
    assert!(assert_fault_parity(&store, now_ms()).is_empty());
    let next = append_history_failure(
        &store, "ordinary", "next", "person/operator", "observed", vec!["daemon/node".into()],
    );
    assert_eq!(assert_fault_parity(&store, now_ms())[0].item.episode, next.id);
    drop(store);
    let reopened = Store::open(&path, "node").unwrap();
    assert_eq!(assert_fault_parity(&reopened, now_ms())[0].item.episode, next.id);
    append_history_recovery(&reopened, &next, "recover-after-reopen");
    assert!(assert_fault_parity(&reopened, now_ms()).is_empty());
}

#[test]
fn a_history_row_decode_error_propagates_instead_of_returning_no_faults() {
    let store = Store::open_memory("node").unwrap();
    let failure = append_history_failure(
        &store, "ordinary", "decode-error", "person/operator", "observed", vec!["daemon/node".into()],
    );
    // Test-only corruption of the row's actor type: JSON and canonical sort stay valid.
    // Earlier reducers have no candidates; claim_from_row fails inside the history fold.
    store.connection.write().execute(
        "UPDATE claims SET actor=x'00' WHERE id=?1", [&failure.id],
    ).unwrap();
    assert!(store.operational_attention_items_with_failures(None, now_ms()).is_err());
    assert!(store.fault_snapshot(now_ms()).is_err());
    assert!(store.broken_gate_items(None, now_ms()).is_err());
    store.connection.write().execute(
        "UPDATE claims SET actor='daemon/runtime' WHERE id=?1", [&failure.id],
    ).unwrap();
    assert_eq!(assert_fault_parity(&store, now_ms()).len(), 1);
}
