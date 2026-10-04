use super::arrangements::*;
use super::tests::{exchange_from, exchange_of, receive_and_project};
use super::*;

const SUBJECT: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000001";
const A: &str = "019a0000-0000-7000-8000-000000000010";
const B: &str = "019a0000-0000-7000-8000-000000000020";
const C: &str = "019a0000-0000-7000-8000-000000000030";
fn input(operations: Value) -> ClaimInput {
    ClaimInput { subject: SUBJECT.into(), kind: "arrangement.edited".into(), actor: Some("person/ada".into()),
        fields: serde_json::from_value(json!({"owner":"person/ada","operations":operations})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None }
}
fn append(store: &Store, operations: Value) -> ClaimRecord { store.append_client_claim(&input(operations)).unwrap() }
fn sync(source: &Store, target: &Store) { receive_and_project(target,&source.origin,&exchange_from(source,&ReplicationInventory::default())); }
fn layout(store: &Store) -> Value { store.arrangement(SUBJECT,u64::MAX).unwrap().unwrap() }
fn create(store: &Store) {
    append(store,json!([{"op":"create","name":"Work"},{"op":"folder.create","id":A,"name":"A","parent":null,"key":"a0"},
        {"op":"folder.create","id":B,"name":"B","parent":null,"key":"a1"}]));
}

#[test]
fn owner_admission_real_agent_and_atomic_identity_fences() {
    let store = Store::open_memory("node").unwrap();
    for actor in [None, Some("person/other"), Some("daemon/runtime")] {
        let mut claim = input(json!([{"op":"create","name":"Work"}]));
        claim.actor = actor.map(str::to_owned);
        assert_eq!(store.append_client_claim(&claim).unwrap_err().code,"arrangement-owner-forbidden");
    }
    let mut wrong_owner = input(json!([{"op":"create","name":"Work"}]));
    wrong_owner.fields.insert("owner".into(),json!("person/other"));
    assert_eq!(store.append_claim(&wrong_owner).unwrap_err().code,"arrangement-owner-forbidden");
    create(&store);
    let before = layout(&store);
    let mut agent = input(json!([{"op":"rename","name":"From agent"}]));
    agent.actor = Some("agent/fleet/seat".into());
    agent.idempotency_key = Some("session/edit-one".into());
    agent.fields.insert("action_id".into(),json!("action-one"));
    agent.fields.insert("action_digest".into(),json!("0".repeat(64)));
    let authority = store.append_claim(&ClaimInput { subject:"resource/authority".into(),kind:"resource.observed".into(),actor:None,
        fields:serde_json::from_value(json!({"kind":"custom.test.authority"})).unwrap(),evidence:vec![],expected_subject:None,idempotency_key:None }).unwrap();
    let mut fences = BTreeMap::from([(SUBJECT.into(),"stale-layout".into()),(authority.subject.clone(),"stale-authority".into())]);
    assert_eq!(store.edit_arrangement(&agent,&fences).unwrap_err().code,"stale-subject");
    assert_eq!(layout(&store),before);
    fences.insert(authority.subject.clone(),authority.id.clone());
    agent.expected_subject = Some(Some("stale-layout".into()));
    let accepted = store.edit_arrangement(&agent,&fences).unwrap();
    assert_eq!(accepted.actor.as_deref(),Some("agent/fleet/seat"));
    assert_eq!(layout(&store)["owner"],"person/ada");
    assert_eq!(layout(&store)["body"]["name"],json!({"value":"From agent","revision":accepted.id}));
    fences.insert(authority.subject.clone(),"now-stale-again".into());
    assert_eq!(store.edit_arrangement(&agent,&fences).unwrap().id,accepted.id);
    let connection = store.readers.get();
    let mut forged = accepted.clone();
    forged.actor = Some("person/other".into());
    assert!(classify_replicated_claim_with_registry(&forged,st3_schema::registry()).is_err());
    let status = subject_status_at(&connection,SUBJECT,None,None).unwrap().unwrap().0;
    let mut changed_action = agent.clone();
    changed_action.fields.insert("action_id".into(),json!("action-two"));
    changed_action.fields.insert("action_digest".into(),json!("1".repeat(64)));
    assert_eq!(store.edit_arrangement(&changed_action,&fences).unwrap_err().code,"idempotency-mismatch");
    assert_eq!(status.owner_run,None);
    assert_eq!(status.actual.unwrap()["owner"],"person/ada");
}

#[test]
fn independent_registers_and_same_register_canonical_max_converge_in_both_orders() {
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open_memory("birch").unwrap();
    create(&a); sync(&a,&b);
    let rename = append(&a,json!([{"op":"folder.rename","id":A,"name":"Renamed"},{"op":"rename","name":"Al"}]));
    let moved = append(&b,json!([{"op":"folder.move","id":A,"parent":B,"key":"a0"},{"op":"rename","name":"Bi"}]));
    let mut envelopes = exchange_from(&a,&ReplicationInventory::default()).envelopes;
    envelopes.extend(exchange_from(&b,&ReplicationInventory::default()).envelopes);
    let c = Store::open_memory("cedar").unwrap();
    let d = Store::open_memory("elm").unwrap();
    for envelope in &envelopes { receive_and_project(&c,"relay",&exchange_of("relay",vec![envelope.clone()])); }
    for envelope in envelopes.iter().rev() { receive_and_project(&d,"relay",&exchange_of("relay",vec![envelope.clone()])); }
    let resource = layout(&c);
    assert_eq!(resource,layout(&d));
    assert_eq!(resource["body"]["folders"][A]["name"],json!({"value":"Renamed","revision":rename.id}));
    assert_eq!(resource["body"]["folders"][A]["position"],json!({"value":{"parent":B,"key":"a0"},"revision":moved.id}));
    let connection = c.readers.get();
    let expected = if canonical::claim_key(&connection,&rename.id).unwrap() > canonical::claim_key(&connection,&moved.id).unwrap() { &rename } else { &moved };
    assert_eq!(resource["body"]["name"]["revision"],expected.id);
    drop(connection);
    let before = c.replication_status(true,None,&[]).unwrap().projection_digests;
    c.rebuild_claim_projections().unwrap();
    assert_eq!(resource,layout(&c));
    assert_eq!(before,c.replication_status(true,None,&[]).unwrap().projection_digests);
    let directory = tempfile::tempdir().unwrap();
    let (plan,proof) = c.plan_checkpoint(now_ms()+1,directory.path()).unwrap();
    assert!(proof.passed);
    assert!(plan.claims.iter().all(|claim| claim.kind != "arrangement.edited"));
}

#[test]
fn local_descendant_moves_refuse_but_concurrent_cycles_cut_greatest_position() {
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open_memory("birch").unwrap();
    create(&a); sync(&a,&b);
    let move_a = append(&a,json!([{"op":"folder.move","id":A,"parent":B,"key":"a0"}]));
    let before = layout(&a);
    assert_eq!(a.append_claim(&input(json!([{"op":"folder.move","id":B,"parent":A,"key":"a1"}]))).unwrap_err().code,"arrangement-cycle");
    assert_eq!(layout(&a),before);
    let move_b = append(&b,json!([{"op":"folder.move","id":B,"parent":A,"key":"a1"}]));
    sync(&a,&b); sync(&b,&a);
    let resource = layout(&a);
    assert_eq!(resource,layout(&b));
    assert_eq!(resource["body"]["folders"][A]["position"]["value"]["parent"],B);
    assert_eq!(resource["body"]["folders"][B]["position"]["value"]["parent"],A);
    let connection = a.readers.get();
    let cut = if canonical::claim_key(&connection,&move_a.id).unwrap() > canonical::claim_key(&connection,&move_b.id).unwrap() { A } else { B };
    let child = if cut == A { B } else { A };
    assert!(resource["resolved"]["parents"][cut].is_null());
    assert_eq!(resource["resolved"]["parents"][child],cut);
}

#[test]
fn permanent_tombstones_lift_ancestors_and_keep_orphan_placements() {
    let a = Store::open_memory("alder").unwrap();
    let b = Store::open_memory("birch").unwrap();
    create(&a);
    append(&a,json!([{"op":"folder.move","id":B,"parent":A,"key":"a0"},
        {"op":"folder.create","id":C,"name":"Child","parent":B,"key":"a0"},
        {"op":"subject.place","subject":"agent/missing/seat","folder":B,"key":"a0"},
        {"op":"subject.place","subject":"agent/missing/other","folder":"019a0000-0000-7000-8000-000000000099","key":"a1"}]));
    sync(&a,&b);
    append(&a,json!([{"op":"folder.delete","id":B}]));
    append(&b,json!([{"op":"folder.rename","id":B,"name":"Offline"}]));
    sync(&a,&b); sync(&b,&a);
    let resource = layout(&a);
    assert_eq!(resource,layout(&b));
    assert_eq!(resource["body"]["folders"][B]["tombstone"]["value"],true);
    assert_eq!(resource["body"]["folders"][B]["position"]["value"]["parent"],A);
    assert_eq!(resource["resolved"]["parents"][C],A);
    assert_eq!(resource["resolved"]["folders"]["agent/missing/seat"],A);
    assert!(resource["resolved"]["folders"]["agent/missing/other"].is_null());
    assert_eq!(resource["body"]["placements"]["agent/missing/seat"]["value"]["folder"],B);
    assert_eq!(a.append_claim(&input(json!([{"op":"folder.create","id":B,"name":"Reuse","parent":null,"key":"a0"}]))).unwrap_err().code,"arrangement-folder-exists");
    let mut retire = input(json!([{"op":"retire"}]));
    retire.actor = Some("agent/fleet/seat".into());
    a.append_claim(&retire).unwrap();
    append(&b,json!([{"op":"rename","name":"Offline after retirement"}]));
    sync(&a,&b); sync(&b,&a);
    assert!(a.arrangement(SUBJECT,u64::MAX).unwrap().is_none());
    assert!(b.arrangements("person/ada",u64::MAX).unwrap().is_empty());
    assert_eq!(a.append_claim(&input(json!([{"op":"create","name":"Reuse"}]))).unwrap_err().code,"arrangement-retired");
    b.rebuild_claim_projections().unwrap();
    assert!(b.arrangement(SUBJECT,u64::MAX).unwrap().is_none());
    let directory = tempfile::tempdir().unwrap();
    let (plan,proof) = a.plan_checkpoint(now_ms()+1,directory.path()).unwrap();
    assert!(proof.passed);
    assert!(plan.claims.iter().all(|claim| claim.kind != "arrangement.edited"));
}

#[test]
fn reads_and_write_validation_do_not_need_claim_history() {
    let store = Store::open_memory("node").unwrap();
    create(&store);
    let before = layout(&store);
    assert!(store.arrangement(SUBJECT,0).is_err());
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    // If either path consults edit history this fails, rather than merely asserting query text.
    transaction.execute_batch("ALTER TABLE claims RENAME TO unavailable_history").unwrap();
    assert_eq!(arrangement_at(&transaction,SUBJECT,u64::MAX).unwrap().unwrap(),before);
    assert_eq!(arrangements_at(&transaction,"person/ada",u64::MAX).unwrap(),vec![before.clone()]);
    prepare(&transaction,&input(json!([{"op":"folder.rename","id":A,"name":"New"}]))).unwrap();
    assert_eq!(subject_status_at(&transaction,SUBJECT,None,None).unwrap().unwrap().0.actual,Some(before));
    transaction.rollback().unwrap();
}

#[test]
fn projected_resource_size_is_admitted_atomically_and_retirement_remains_possible() {
    let store = Store::open_memory("node").unwrap();
    create(&store);
    let before = layout(&store);
    // Bound subject refs individually, but make their combined resource exceed the read budget.
    let operations: Vec<_> = (0..1024).map(|id| json!({"op":"subject.place","subject":format!("agent/{id}/{}","x".repeat(430)),"folder":null,"key":"a0"})).collect();
    assert_eq!(store.append_claim(&input(json!(operations))).unwrap_err().code,"arrangement-body-too-large");
    assert_eq!(layout(&store),before);
    append(&store,json!([{"op":"retire"}]));
    assert!(store.arrangement(SUBJECT,u64::MAX).unwrap().is_none());
}

#[test]
fn heads_survive_reopen_and_an_upgrade_backfills_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("arrangements.sqlite");
    let store = Store::open(&path,"node").unwrap();
    create(&store);
    let mut edit = input(json!([{"op":"subject.place","subject":"agent/away/seat","folder":A,"key":"a0"}]));
    edit.actor = Some("agent/fleet/seat".into());
    store.append_claim(&edit).unwrap();
    let before = layout(&store);
    {
        let connection = store.connection.write();
        connection.execute_batch("DELETE FROM arrangement_registers; DELETE FROM arrangements; DELETE FROM meta WHERE key='arrangement_heads_v1'").unwrap();
    }
    drop(store);
    let store = Store::open(&path,"node").unwrap();
    assert_eq!(layout(&store),before);
    drop(store);
    let store = Store::open(&path,"node").unwrap();
    assert_eq!(layout(&store),before);
    let replica = Store::open_memory("peer").unwrap();
    sync(&store,&replica);
    assert_eq!(layout(&replica),before);
}

#[test]
fn atomic_rekeys_validate_final_positions_not_intermediate_descendants() {
    let store = Store::open_memory("node").unwrap();
    create(&store);
    append(&store,json!([{"op":"folder.move","id":B,"parent":A,"key":"a0"}]));
    // The first move alone would make a cycle, but the second simultaneously lifts B out.
    let accepted = append(&store,json!([
        {"op":"folder.move","id":A,"parent":B,"key":"a0"},
        {"op":"folder.move","id":B,"parent":null,"key":"a1"},
    ]));
    let resource = layout(&store);
    assert_eq!(resource["resolved"]["parents"][A],B);
    assert!(resource["resolved"]["parents"][B].is_null());
    assert_eq!(resource["body"]["folders"][A]["position"]["revision"],accepted.id);
    assert_eq!(resource["body"]["folders"][B]["position"]["revision"],accepted.id);
    // Delete/rename touch independent registers and must not depend on operation order.
    append(&store,json!([{"op":"folder.delete","id":A},{"op":"folder.rename","id":A,"name":"Archived"}]));
    let resource = layout(&store);
    assert_eq!(resource["body"]["folders"][A]["name"]["value"],"Archived");
    assert_eq!(resource["body"]["folders"][A]["tombstone"]["value"],true);
}

#[test]
fn first_agent_action_uses_normal_principal_signing_without_changing_person_ownership() {
    let store = Store::open_memory("node").unwrap();
    store.set_node_key(Arc::new(smallclaims::fleet::MemberKey::generate().unwrap().0)).unwrap();
    let mut creation = input(json!([{"op":"create","name":"Agent created"}]));
    creation.actor = Some("agent/fleet/new-seat".into());
    let accepted = store.edit_arrangement(&creation,&BTreeMap::new()).unwrap();
    assert_eq!(accepted.actor.as_deref(),Some("agent/fleet/new-seat"));
    store.replication_snapshot().unwrap(); // Publishing seals the batch and signs its claims.
    let signature = store.claim_signature(&accepted.id).unwrap().unwrap();
    assert_eq!(signature.signer,"agent/fleet/new-seat");
    assert_eq!(signature.on_behalf,None);
    assert_eq!(layout(&store)["owner"],"person/ada");
}

#[test]
fn checkpoint_retains_superseded_agent_edits_and_proves_rebuilt_head_readers() {
    let store = Store::open_memory("node").unwrap();
    create(&store);
    let mut agent = input(json!([{"op":"rename","name":"Agent's first edit"}]));
    agent.actor = Some("agent/fleet/seat".into());
    let older = store.append_claim(&agent).unwrap();
    agent.fields.insert("operations".into(),json!([{"op":"rename","name":"Agent's newer edit"},{"op":"subject.place","subject":"agent/away/seat","folder":A,"key":"a0"}]));
    let newer = store.append_claim(&agent).unwrap();
    // Neither agent edit is a person claim or belongs to the newest envelope.
    for number in 0..3 {
        store.append_claim(&ClaimInput {
            subject:"resource/checkpoint-noise".into(),kind:"resource.observed".into(),actor:Some("daemon/runtime".into()),
            fields:serde_json::from_value(json!({"kind":"custom.test.checkpoint","facts":{"number":number}})).unwrap(),
            evidence:vec![],expected_subject:None,idempotency_key:None,
        }).unwrap();
    }
    let before = layout(&store);
    store.rebuild_claim_projections().unwrap();
    assert_eq!(layout(&store),before);
    let directory = tempfile::tempdir().unwrap();
    let (plan,proof) = store.plan_checkpoint(now_ms()+1,directory.path()).unwrap();
    assert!(proof.passed);
    assert!(plan.claims.iter().all(|claim| claim.kind != "arrangement.edited"));
    assert_eq!(store.claim_by_id(&older.id).unwrap().unwrap().actor.as_deref(),Some("agent/fleet/seat"));
    assert_eq!(store.claim_by_id(&newer.id).unwrap().unwrap().actor.as_deref(),Some("agent/fleet/seat"));
    assert_eq!(layout(&store),before);
}
