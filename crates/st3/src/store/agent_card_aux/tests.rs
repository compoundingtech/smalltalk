use super::*;
use crate::store::Store;
use serde_json::json;
use smallclaims::ClaimInput;

const NS: &str = "aux-test-v1";
const AGENT: &str = "agent/aux";

fn setup() -> Store {
    let store = Store::open_memory("aux-oracle").unwrap();
    create_schema(&store.connection.lock().unwrap()).unwrap();
    store
}

// Use an actual Store/batch/claim, then replace its fields in this owned fixture. This permits
// historical malformed JSON and duplicate appearances which current admission may reject.
// The production adapter must independently capture and fence every such replacement.
fn input(store: &Store, agent: &str, kind: &str, fields: Value, time: u128) -> ClaimRecord {
    let claim = store
        .append_claim(&ClaimInput {
            subject: "daemon/aux-oracle".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: serde_json::from_value(
                json!({"severity":"warning","code":"aux-fixture","reason":"source oracle"}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    {
        let connection = store.connection.lock().unwrap();
        connection
            .execute(
                "UPDATE claims SET subject=?2,kind=?3,body=?4,accepted_at_unix_ms=?5 WHERE id=?1",
                params![
                    claim.id,
                    agent,
                    kind,
                    json!({"fields":fields}).to_string(),
                    time.to_string()
                ],
            )
            .unwrap();
    }
    store.claim_by_id(&claim.id).unwrap().unwrap()
}
fn key(store: &Store, claim: &ClaimRecord) -> ClaimKey {
    canonical::claim_key(&store.readers.get(), &claim.id).unwrap()
}
fn project(store: &Store, old: Option<&ClaimRecord>, new: Option<&ClaimRecord>) -> Effects {
    let key = new.map(|claim| key(store, claim));
    let mut connection = store.connection.lock().unwrap();
    let tx = connection.transaction().unwrap();
    let effects = apply(&tx, NS, old, new.zip(key.as_ref())).unwrap();
    tx.commit().unwrap();
    effects
}
fn replace(store: &Store, old: &ClaimRecord, agent: &str, fields: Value) -> ClaimRecord {
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE claims SET subject=?2,body=?3 WHERE id=?1",
            params![old.id, agent, json!({"fields":fields}).to_string()],
        )
        .unwrap();
    store.claim_by_id(&old.id).unwrap().unwrap()
}
fn remove(store: &Store, claim: &ClaimRecord) {
    store
        .connection
        .lock()
        .unwrap()
        .execute("DELETE FROM claims WHERE id=?1", [&claim.id])
        .unwrap();
    project(store, Some(claim), None);
}
fn appearance(store: &Store, id: &str, lease: Value, time: u128) -> ClaimRecord {
    input(
        store,
        AGENT,
        "subagent.appeared",
        json!({"subagent_id":id,"driver":"claude","incarnation_id":"i","session_id":"s","description":"task","subagent_type":"Explore","step_run":"step/x","started_at_unix_ms":3,"lease_expires_at_unix_ms":lease}),
        time,
    )
}
fn parity(store: &Store, agent: &str, now: u64) {
    let oracle: Vec<_> = store
        .running_subagents(now)
        .unwrap()
        .into_iter()
        .filter(|row| row.agent == agent)
        .collect();
    let got = running(&store.readers.get(), NS, agent, now).unwrap();
    assert_eq!(got, oracle);
    assert_eq!(
        serde_json::to_value(got).unwrap(),
        serde_json::to_value(oracle).unwrap()
    );
}
fn todo_parity(store: &Store, agent: &str, at: u64) {
    let oracle = store
        .agent_todo_observations_for(&[agent.into()], at)
        .unwrap();
    let got = todo_session(&store.readers.get(), NS, agent).unwrap();
    for (kind, row) in [
        ("harness.todo.observed", got.0),
        ("harness.session-file", got.1),
    ] {
        let want = oracle.get(agent).and_then(|rows| rows.get(kind));
        assert_eq!(
            serde_json::to_value(row).unwrap(),
            serde_json::to_value(want).unwrap()
        );
    }
}
fn fault_parity(store: &Store, agent: &str, at: u64) {
    assert_eq!(
        fault(&store.readers.get(), NS, agent).unwrap(),
        store
            .member_reconcile_faults_for(&[agent.into()], at.min(i64::MAX as u64))
            .unwrap()
            .remove(agent)
    );
}

#[test]
fn real_store_todo_session_use_canonical_heads_and_preserve_full_record() {
    let store = setup();
    let a = input(
        &store,
        AGENT,
        "harness.todo.observed",
        json!({"summary":"newer","items":[{"text":"a","state":"done"}]}),
        100,
    );
    let b = input(
        &store,
        AGENT,
        "harness.todo.observed",
        json!({"summary":"arrived later but older"}),
        50,
    );
    let session = input(
        &store,
        AGENT,
        "harness.session-file",
        json!({"path":"/tmp/s","incarnation_id":"i"}),
        99,
    );
    for row in [&b, &session, &a] {
        project(&store, None, Some(row));
    }
    todo_parity(&store, AGENT, u64::MAX);
    assert_eq!(
        todo_session(&store.readers.get(), NS, AGENT)
            .unwrap()
            .0
            .unwrap()
            .id,
        a.id
    );
    remove(&store, &a);
    todo_parity(&store, AGENT, u64::MAX);
}

#[test]
fn real_store_fault_uses_arrival_not_canonical_and_strict_member_key() {
    let store = setup();
    let a = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"member-fault","reason":"first"}),
        100,
    );
    let b = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"member-fault","reason":false}),
        10,
    );
    let unrelated = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"other","decision":"member-fault","reason":"wrong"}),
        200,
    );
    for row in [&unrelated, &b, &a] {
        project(&store, None, Some(row));
    }
    fault_parity(&store, AGENT, u64::MAX);
    assert_eq!(
        fault(&store.readers.get(), NS, AGENT).unwrap().as_deref(),
        Some("member reconciliation failed")
    );
    let empty = replace(
        &store,
        &b,
        AGENT,
        json!({"key":"member-reconcile","decision":"member-fault","reason":""}),
    );
    project(&store, Some(&b), Some(&empty));
    fault_parity(&store, AGENT, u64::MAX);
    let clear = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"ok"}),
        1,
    );
    project(&store, None, Some(&clear));
    fault_parity(&store, AGENT, u64::MAX);
    remove(&store, &clear);
    fault_parity(&store, AGENT, u64::MAX);
}

#[test]
fn real_store_duplicate_appearances_renewal_any_end_and_inclusive_expiry() {
    let store = setup();
    let a = appearance(&store, "same", json!(10), 100);
    let b = appearance(&store, "same", json!(20), 50);
    let renewal = input(
        &store,
        AGENT,
        "subagent.renewed",
        json!({"subagent_id":"same","lease_expires_at_unix_ms":"30ms"}),
        5,
    );
    for row in [&renewal, &a, &b] {
        project(&store, None, Some(row));
    }
    for now in [0, 9, 10, 19, 20, 29, 30, 31] {
        parity(&store, AGENT, now);
    }
    assert_eq!(
        running(&store.readers.get(), NS, AGENT, 0)
            .unwrap()
            .iter()
            .map(|row| row.appeared.clone())
            .collect::<Vec<_>>(),
        [b.id.clone(), a.id.clone()]
    );
    assert_eq!(
        deadline(&store.readers.get(), NS, AGENT, 0).unwrap(),
        Some(30)
    );
    let end = input(
        &store,
        AGENT,
        "subagent.ended",
        json!({"subagent_id":"same"}),
        1,
    );
    project(&store, None, Some(&end));
    parity(&store, AGENT, 0);
    remove(&store, &end);
    parity(&store, AGENT, 0);
    remove(&store, &renewal);
    parity(&store, AGENT, 10);
    assert_eq!(
        deadline(&store.readers.get(), NS, AGENT, 10).unwrap(),
        Some(20)
    );
}

#[test]
fn real_store_sqlite_lease_cast_and_public_json_u64_semantics() {
    let store = setup();
    for (n, lease) in [
        json!(-20),
        json!(0),
        json!(9),
        json!(11.5),
        json!("12.9"),
        json!("13x"),
        json!(false),
        json!(null),
        json!(u64::MAX),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("s{n}");
        let row = appearance(&store, &id, lease, (n + 1) as u128);
        project(&store, None, Some(&row));
        let renew = input(
            &store,
            AGENT,
            "subagent.renewed",
            json!({"subagent_id":id,"lease_expires_at_unix_ms":if n%2==0 {json!("15x")} else {json!(-1)}}),
            50 + n as u128,
        );
        project(&store, None, Some(&renew));
    }
    for now in [
        0,
        9,
        11,
        12,
        14,
        15,
        16,
        i64::MAX as u64 - 1,
        i64::MAX as u64,
        u64::MAX,
    ] {
        parity(&store, AGENT, now);
    }
}

#[test]
fn real_store_json_type_equality_renews_and_ends_literal_json_text_ids() {
    let store = setup();
    let a = appearance(&store, "[1]", json!(10), 1);
    project(&store, None, Some(&a));
    let renewal = input(
        &store,
        AGENT,
        "subagent.renewed",
        json!({"subagent_id":[1],"lease_expires_at_unix_ms":40}),
        2,
    );
    project(&store, None, Some(&renewal));
    let numeric = input(
        &store,
        AGENT,
        "subagent.appeared",
        json!({"subagent_id":1,"lease_expires_at_unix_ms":40}),
        3,
    );
    project(&store, None, Some(&numeric));
    let text = appearance(&store, "1", json!(10), 4);
    project(&store, None, Some(&text));
    parity(&store, AGENT, 20);
    let end = input(
        &store,
        AGENT,
        "subagent.ended",
        json!({"subagent_id":[1]}),
        5,
    );
    project(&store, None, Some(&end));
    parity(&store, AGENT, 0);
}

#[test]
fn old_new_retarget_moves_subagent_and_heads_without_stale_rows() {
    let store = setup();
    let a = appearance(&store, "old", json!(100), 1);
    project(&store, None, Some(&a));
    let b = replace(
        &store,
        &a,
        "agent/other",
        json!({"subagent_id":"new","lease_expires_at_unix_ms":50}),
    );
    let effects = project(&store, Some(&a), Some(&b));
    assert_eq!(effects.agents.len(), 2);
    parity(&store, AGENT, 0);
    parity(&store, "agent/other", 0);
    let t = input(&store, AGENT, "harness.todo.observed", json!({"x":1}), 5);
    project(&store, None, Some(&t));
    let moved = replace(&store, &t, "agent/other", json!({"x":2}));
    project(&store, Some(&t), Some(&moved));
    todo_parity(&store, AGENT, u64::MAX);
    todo_parity(&store, "agent/other", u64::MAX);
}

#[test]
fn checkpoint_arrival_renumbering_requires_and_honors_explicit_redispatch() {
    let store = setup();
    let a = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"member-fault","reason":"a"}),
        2,
    );
    let b = input(
        &store,
        AGENT,
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"member-fault","reason":"b"}),
        1,
    );
    project(&store, None, Some(&a));
    project(&store, None, Some(&b));
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE claims SET store_index=store_index+1000 WHERE id=?1 OR id=?2",
            params![a.id, b.id],
        )
        .unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE claims SET store_index=?2 WHERE id=?1",
            params![a.id, b.store_index],
        )
        .unwrap();
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE claims SET store_index=?2 WHERE id=?1",
            params![b.id, a.store_index],
        )
        .unwrap();
    let newa = store.claim_by_id(&a.id).unwrap().unwrap();
    let newb = store.claim_by_id(&b.id).unwrap().unwrap();
    project(&store, Some(&a), Some(&newa));
    project(&store, Some(&b), Some(&newb));
    fault_parity(&store, AGENT, u64::MAX);
    assert_eq!(
        fault(&store.readers.get(), NS, AGENT).unwrap().as_deref(),
        Some("a")
    );
}

#[test]
fn authorized_snapshot_cut_admits_heads_only_when_cut_advances() {
    let store = setup();
    let old = input(
        &store,
        AGENT,
        "harness.todo.observed",
        json!({"at":"old"}),
        1,
    );
    project(&store, None, Some(&old));
    let new = input(
        &store,
        AGENT,
        "harness.todo.observed",
        json!({"at":"new"}),
        2,
    );
    todo_parity(&store, AGENT, old.store_index);
    project(&store, None, Some(&new));
    todo_parity(&store, AGENT, new.store_index);
}

#[test]
fn transaction_refusal_rolls_back_cache_counters_and_outputs_together() {
    let store = setup();
    let row = appearance(&store, "a", json!(100), 1);
    project(&store, None, Some(&row));
    let end = input(
        &store,
        AGENT,
        "subagent.ended",
        json!({"subagent_id":"a"}),
        2,
    );
    let rank = key(&store, &end);
    {
        let mut connection = store.connection.lock().unwrap();
        let tx = connection.transaction().unwrap();
        apply(&tx, NS, None, Some((&end, &rank))).unwrap();
        assert!(running(&tx, NS, AGENT, 0).unwrap().is_empty());
        tx.rollback().unwrap();
    }
    assert_eq!(
        running(&store.readers.get(), NS, AGENT, 0).unwrap().len(),
        1
    );
    assert!(available(&store.readers.get(), NS, AGENT).unwrap());
}

#[test]
fn namespace_isolation_and_pending_refuse_every_family() {
    let store = setup();
    let row = appearance(&store, "a", json!(100), 1);
    project(&store, None, Some(&row));
    assert!(
        running(&store.readers.get(), "other", AGENT, 0)
            .unwrap()
            .is_empty()
    );
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE local_agent_card_aux_agents SET pending=1 WHERE namespace=?1 AND agent=?2",
            params![NS, AGENT],
        )
        .unwrap();
    let connection = store.readers.get();
    assert!(!available(&connection, NS, AGENT).unwrap());
    assert!(running(&connection, NS, AGENT, 0).is_err());
    assert!(todo_session(&connection, NS, AGENT).is_err());
    assert!(fault(&connection, NS, AGENT).is_err());
    assert!(deadline(&connection, NS, AGENT, 0).is_err());
}

#[test]
fn per_id_exhaustion_refuses_instead_of_truncating_and_any_end_closes_group() {
    let store = setup();
    let mut rows = vec![];
    for n in 0..=APPEARANCES_PER_ID {
        let row = appearance(&store, "duplicates", json!(100), n as u128);
        project(&store, None, Some(&row));
        rows.push(row);
    }
    assert!(!available(&store.readers.get(), NS, AGENT).unwrap());
    assert!(running(&store.readers.get(), NS, AGENT, 0).is_err());
    let end = input(
        &store,
        AGENT,
        "subagent.ended",
        json!({"subagent_id":"duplicates"}),
        0,
    );
    project(&store, None, Some(&end));
    parity(&store, AGENT, 0);
    remove(&store, &end);
    assert!(!available(&store.readers.get(), NS, AGENT).unwrap());
    remove(&store, &rows[0]);
    parity(&store, AGENT, 0);
}

#[test]
fn complete_agent_array_bound_refuses_and_retraction_recovers_exactly() {
    let store = setup();
    let mut last = None;
    for n in 0..=OPEN_ROWS_PER_AGENT {
        let row = appearance(&store, &format!("s{n}"), json!(100), n as u128);
        project(&store, None, Some(&row));
        last = Some(row);
    }
    assert!(running(&store.readers.get(), NS, AGENT, 0).is_err());
    assert!(todo_session(&store.readers.get(), NS, AGENT).is_err());
    remove(&store, &last.unwrap());
    parity(&store, AGENT, 0);
}

#[test]
fn oversized_source_and_invalid_captured_key_persist_explicit_refusal() {
    let store = setup();
    let row = input(
        &store,
        AGENT,
        "harness.todo.observed",
        json!({"text":"x".repeat(RECORD_BYTES)}),
        1,
    );
    assert!(
        project(&store, None, Some(&row))
            .unavailable
            .contains(AGENT)
    );
    assert!(todo_session(&store.readers.get(), NS, AGENT).is_err());
    remove(&store, &row);
    assert!(!available(&store.readers.get(), NS, AGENT).unwrap());
    let row = appearance(&store, "a", json!(100), 2);
    let mut rank = key(&store, &row);
    rank.5 = "wrong".into();
    let mut connection = store.connection.lock().unwrap();
    let tx = connection.transaction().unwrap();
    let result = apply(&tx, "bad-key", None, Some((&row, &rank))).unwrap();
    assert!(result.unavailable.contains(AGENT));
    tx.commit().unwrap();
}

#[test]
fn long_history_uses_indexed_heads_and_renewals_without_history_on_reads() {
    let store = setup();
    let a = appearance(&store, "same", json!(100), 1);
    project(&store, None, Some(&a));
    for n in 0..128 {
        let row = input(
            &store,
            AGENT,
            "subagent.renewed",
            json!({"subagent_id":"same","lease_expires_at_unix_ms":200+n}),
            n as u128,
        );
        project(&store, None, Some(&row));
    }
    parity(&store, AGENT, 250);
    let connection = store.readers.get();
    for (query, index) in [
        (
            "SELECT record FROM local_agent_card_aux_nodes WHERE namespace='aux-test-v1' AND agent='agent/aux' AND kind='harness.todo.observed' ORDER BY rank DESC LIMIT 1",
            "aux_canonical",
        ),
        (
            "SELECT record FROM local_agent_card_aux_nodes WHERE namespace='aux-test-v1' AND agent='agent/aux' AND kind='runtime.reconcile-decision' AND fault_eligible=1 ORDER BY arrival DESC LIMIT 1",
            "aux_arrival",
        ),
        (
            "SELECT lease_cast FROM local_agent_card_aux_nodes WHERE namespace='aux-test-v1' AND agent='agent/aux' AND subid='same' AND kind='subagent.renewed' ORDER BY lease_cast DESC LIMIT 1",
            "aux_group_lease",
        ),
        (
            "SELECT expires FROM local_agent_card_aux_subagents WHERE namespace='aux-test-v1' AND agent='agent/aux' AND expires>0 ORDER BY expires LIMIT 1",
            "aux_subagents_due",
        ),
    ] {
        let mut s = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
            .unwrap();
        let plans = s
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(plans.iter().any(|plan| plan.contains(index)), "{plans:?}");
        assert!(
            !plans
                .iter()
                .any(|plan| plan.contains("SCAN ") || plan.contains("TEMP B-TREE")),
            "{plans:?}"
        );
    }
}

#[test]
fn staged_reverse_keys_are_removed_even_when_declared_old_metadata_differs() {
    let store = setup();
    let a = appearance(&store, "original", json!(100), 1);
    project(&store, None, Some(&a));
    let mut supplied_old = a.clone();
    supplied_old.subject = "agent/intermediate".into();
    supplied_old.body =
        json!({"fields":{"subagent_id":"intermediate","lease_expires_at_unix_ms":100}});
    let replacement = replace(
        &store,
        &a,
        "agent/final",
        json!({"subagent_id":"final","lease_expires_at_unix_ms":100}),
    );
    let effects = project(&store, Some(&supplied_old), Some(&replacement));
    assert_eq!(effects.agents.len(), 3);
    parity(&store, AGENT, 0);
    parity(&store, "agent/intermediate", 0);
    parity(&store, "agent/final", 0);
}
