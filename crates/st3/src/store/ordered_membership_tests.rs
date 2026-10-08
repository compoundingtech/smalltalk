use super::tests::{exchange_from, exchange_of, receive_and_project};
use super::*;

const CONTAINER: &str = "arrangement/person/ada/019a0000-0000-7000-8000-000000000101";
const FOLDER_A: &str = "019a0000-0000-7000-8000-000000000110";
const FOLDER_B: &str = "019a0000-0000-7000-8000-000000000120";
const SEAT_A: &str = "agent/ada/seat-a";
const SEAT_B: &str = "agent/ada/seat-b";
const SEAT_C: &str = "agent/ada/seat-c";

fn claim(subject: &str, kind: &str, fields: Value) -> ClaimInput {
    ClaimInput {
        subject: subject.into(),
        kind: kind.into(),
        actor: Some("person/ada".into()),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }
}

fn arrangement_input(operations: Value) -> ClaimInput {
    claim(CONTAINER, "arrangement.edited", json!({"owner":"person/ada","operations":operations}))
}

fn create(store: &Store) {
    let mut input = arrangement_input(json!([{"op":"create","name":"Work"}]));
    input.fields.insert("version".into(), json!(2));
    store.edit_arrangement(&input, &BTreeMap::new()).unwrap();
}

fn folder_edit(store: &Store, operations: Value) -> ClaimRecord {
    store.edit_arrangement(&arrangement_input(operations), &BTreeMap::new()).unwrap()
}

fn membership_input(operations: Value) -> ClaimInput {
    claim(CONTAINER, "ordered-membership.edited", json!({"owner":"person/ada","operations":operations}))
}

fn edit(store: &Store, operations: Value) -> ClaimRecord {
    store.edit_ordered_memberships(&membership_input(operations), &BTreeMap::new()).unwrap()
}

fn place(member: &str, bucket: Option<&str>, key: &str) -> Value {
    json!({"op":"place","member":member,"bucket":bucket,"key":key})
}

fn publish(store: &Store, kdl: String, key: &str) {
    let intent = crate::graph::parse_test_intent(&kdl, &store.origin).unwrap();
    let preview = store.mission(&intent, IntentInput { kdl, source_name: None }).unwrap();
    store.apply(&intent, &preview.subject_tokens, key).unwrap();
}

fn declare(store: &Store, member: &str, command: &str, key: &str) {
    let id = member.strip_prefix("agent/").unwrap();
    publish(store, format!("version 2\nagent {id:?} {{ workspace \"/tmp\"; command {command:?}; }}\n"), key);
}

fn retire(store: &Store, member: &str, key: &str) {
    publish(store, format!("version 2\nstop {member:?}\n"), key);
}

fn rows(store: &Store) -> Vec<Value> {
    store.ordered_memberships(CONTAINER, u64::MAX, None, 1000).unwrap()
}

fn members(store: &Store) -> Vec<String> {
    rows(store).into_iter().map(|row| row["member"].as_str().unwrap().to_owned()).collect()
}

// Inspect retained authority without coupling the test to the canonical key's SQL encoding.
fn head(store: &Store, member: &str) -> (Option<Value>, String, String) {
    let (position, revision, winner): (Option<String>, String, String) = store.readers.get().query_row(
        "SELECT position,revision,quote(winner) FROM ordered_membership_heads WHERE container=?1 AND member=?2",
        params![CONTAINER, member],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    (position.map(|value| serde_json::from_str(&value).unwrap()), revision, winner)
}

fn lifecycle(store: &Store, member: &str) -> (bool, String) {
    store.readers.get().query_row(
        "SELECT visible,revision FROM ordered_membership_lifecycle WHERE subject=?1",
        [member], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap()
}

fn sync(source: &Store, target: &Store) {
    receive_and_project(target, &source.origin, &exchange_from(source, &ReplicationInventory::default()));
}

fn membership_digests(store: &Store) -> BTreeMap<String, String> {
    let digests = store.replication_status(true, None, &[]).unwrap().projection_digests;
    let selected: BTreeMap<_, _> = digests.into_iter().filter(|(table, _)| table.starts_with("ordered_membership")).collect();
    for table in ["ordered_membership_heads", "ordered_membership_live", "ordered_membership_counts", "ordered_membership_lifecycle"] {
        assert!(selected.contains_key(table), "missing checkpoint/projection witness for {table}");
    }
    selected
}

fn layout_digests(store: &Store) -> BTreeMap<String, String> {
    store.replication_status(true, None, &[]).unwrap().projection_digests.into_iter()
        .filter(|(table, _)| table.starts_with("ordered_membership")
            || matches!(table.as_str(), "arrangements" | "arrangement_registers" | "declared_resource_edges"))
        .collect()
}

fn repair(store: &Store, original: &ClaimRecord, replacement: &ClaimRecord, key: &str) {
    let record_ref = {
        let connection = store.connection.write();
        let record_ref: String = connection.query_row(
            "SELECT record_ref FROM replica_records WHERE claim_id=?1", [&original.id], |row| row.get(0),
        ).unwrap();
        // Model a receiver whose newer registry rejects a previously admitted operation.
        connection.execute("UPDATE replica_records SET state='unknown' WHERE record_ref=?1", [&record_ref]).unwrap();
        record_ref
    };
    store.repair_replica_record(&record_ref, &replacement.id, "receiver rejects the original operation",
        "person/ada", key).unwrap();
}

#[test]
fn document_binding_declares_the_exact_document_member_identity() {
    let store = Store::open_memory("membership-document").unwrap();
    create(&store);
    edit(&store, json!([place("doc/member", None, "a0")]));
    assert!(rows(&store).is_empty());
    let frontier = store.index().unwrap();
    store.put_document("doc/member", b"bound document", &None, "document-member").unwrap();
    assert_eq!(members(&store), ["doc/member"]);
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 1);
    assert!(store.ordered_memberships_changed(frontier, store.index().unwrap()).unwrap());
    let before = membership_digests(&store);
    store.rebuild_claim_projections().unwrap();
    assert_eq!(membership_digests(&store), before);
}

#[test]
fn pair_projection_before_create_preserves_container_revision_and_restores_visibility() {
    let store = Store::open_memory("membership-create-order").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-before-pair");
    let placed = edit(&store, json!([place(SEAT_A, None, "a0")]));
    let created = store.claims_for(CONTAINER, Some("arrangement.edited")).unwrap().remove(0);
    let before = store.arrangement(CONTAINER, u64::MAX).unwrap();
    let digests = membership_digests(&store);
    let mut connection = store.connection.write();
    let transaction = connection.transaction().unwrap();
    transaction.execute_batch(
        "DELETE FROM ordered_membership_live; DELETE FROM ordered_membership_heads;
         DELETE FROM ordered_membership_counts; DELETE FROM ordered_membership_lifecycle;
         DELETE FROM declared_resource_edges WHERE relation='ordered-membership';
         DELETE FROM arrangement_registers; DELETE FROM arrangements;",
    ).unwrap();
    ordered_membership::project(&transaction, &placed).unwrap();
    assert!(arrangements::arrangement_at(&transaction, CONTAINER, u64::MAX).unwrap().is_none());
    arrangements::project(&transaction, &created).unwrap();
    ordered_membership::flush(&transaction).unwrap();
    assert_eq!(arrangements::arrangement_at(&transaction, CONTAINER, u64::MAX).unwrap(), before);
    assert_eq!(ordered_membership::items_at(&transaction, CONTAINER, u64::MAX, None, 10).unwrap()[0]["member"], SEAT_A);
    transaction.commit().unwrap();
    drop(connection);
    assert_eq!(membership_digests(&store), digests);
}

#[test]
fn retirement_hides_but_same_identity_redeclaration_restores_retained_position() {
    let store = Store::open_memory("membership-lifecycle").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-a");
    let placed = edit(&store, json!([place(SEAT_A, None, "a5")]));
    let retained = head(&store, SEAT_A);
    assert_eq!(retained.0, Some(json!({"bucket":null,"key":"a5"})));
    assert_eq!(retained.1, placed.id);
    let declared = lifecycle(&store, SEAT_A);
    assert!(declared.0);
    assert!(!declared.1.is_empty());
    let frontier = store.index().unwrap();
    assert!(!store.ordered_memberships_changed(frontier, frontier).unwrap());

    retire(&store, SEAT_A, "retire-a");
    assert!(rows(&store).is_empty());
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    assert_eq!(head(&store, SEAT_A), retained);
    let retired = lifecycle(&store, SEAT_A);
    assert!(!retired.0);
    assert_ne!(retired.1, declared.1);
    let pending: u64 = store.readers.get().query_row("SELECT COUNT(*) FROM local_ordered_membership_pending", [], |row| row.get(0)).unwrap();
    assert_eq!(pending, 0, "lifecycle fanout is drained in the declaration transaction");
    assert!(store.ordered_memberships_changed(frontier, store.index().unwrap()).unwrap());
    let retirement = store.index().unwrap();

    declare(&store, SEAT_A, "echo rearmed", "redeclare-a");
    assert_eq!(members(&store), vec![SEAT_A]);
    assert_eq!(rows(&store)[0], json!({"id":SEAT_A,"container":CONTAINER,"member":SEAT_A,
        "position":{"bucket":null,"key":"a5"},"revision":placed.id}));
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 1);
    assert_eq!(head(&store, SEAT_A), retained);
    let restored = lifecycle(&store, SEAT_A);
    assert!(restored.0);
    assert_ne!(restored.1, retired.1);
    assert!(store.ordered_memberships_changed(retirement, store.index().unwrap()).unwrap());
}

#[test]
fn runtime_stopped_suspended_and_running_do_not_retire_a_declared_member() {
    let store = Store::open_memory("membership-runtime").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-a");
    edit(&store, json!([place(SEAT_A, None, "a0")]));
    let retained = head(&store, SEAT_A);
    let declared = lifecycle(&store, SEAT_A);
    for status in ["stopped", "suspended", "running", "exited", "absent"] {
        store.append_claim(&claim(SEAT_A, "runtime.observed", json!({"status":status,"runtime_id":"native"}))).unwrap();
        assert_eq!(members(&store), vec![SEAT_A], "runtime status {status} is not declaration retirement");
        assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 1);
        assert_eq!(head(&store, SEAT_A), retained);
        assert_eq!(lifecycle(&store, SEAT_A), declared);
    }
    retire(&store, SEAT_A, "retire-a");
    store.append_claim(&claim(SEAT_A, "runtime.observed", json!({"status":"running","runtime_id":"late-runtime"}))).unwrap();
    assert!(rows(&store).is_empty(), "a runtime observation cannot resurrect a retired declaration");
    assert_eq!(head(&store, SEAT_A), retained);
}

#[test]
fn unknown_member_is_retained_until_declaration_and_explicit_remove_stays_absent() {
    let store = Store::open_memory("membership-unknown").unwrap();
    create(&store);
    let placed = edit(&store, json!([place(SEAT_A, None, "a0")]));
    assert!(rows(&store).is_empty());
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    assert_eq!(head(&store, SEAT_A).1, placed.id);
    assert_eq!(lifecycle(&store, SEAT_A), (false, String::new()));
    let frontier = store.index().unwrap();
    declare(&store, SEAT_A, "true", "declare-a");
    assert_eq!(members(&store), vec![SEAT_A]);
    assert!(store.ordered_memberships_changed(frontier, store.index().unwrap()).unwrap());
    let removed = edit(&store, json!([{"op":"remove","member":SEAT_A}]));
    let retained = head(&store, SEAT_A);
    assert!(retained.0.is_none());
    assert_eq!(retained.1, removed.id);
    retire(&store, SEAT_A, "retire-a");
    declare(&store, SEAT_A, "echo again", "redeclare-a");
    assert!(rows(&store).is_empty());
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    assert_eq!(head(&store, SEAT_A), retained);
}

#[test]
fn remove_winner_prevents_delayed_older_replicated_place_resurrection() {
    let source = Store::open_memory("membership-source").unwrap();
    create(&source);
    declare(&source, SEAT_A, "true", "declare-a");
    let placed = edit(&source, json!([place(SEAT_A, None, "a0")]));
    let older = exchange_from(&source, &ReplicationInventory::default());
    let removed = edit(&source, json!([{"op":"remove","member":SEAT_A}]));
    let newer = exchange_from(&source, &ReplicationInventory::default());
    let target = Store::open_memory("membership-target").unwrap();
    for envelope in newer.envelopes.iter().rev() {
        receive_and_project(&target, "relay", &exchange_of("relay", vec![envelope.clone()]));
    }
    receive_and_project(&target, &source.origin, &older);
    assert!(rows(&target).is_empty());
    assert_eq!(head(&target, SEAT_A), head(&source, SEAT_A));
    assert_eq!(head(&target, SEAT_A).1, removed.id);
    assert!(head(&target, SEAT_A).0.is_none());
    assert!(target.claim_by_id(&placed.id).unwrap().is_some());
    target.rebuild_claim_projections().unwrap();
    assert!(rows(&target).is_empty());
    assert_eq!(head(&target, SEAT_A).1, removed.id);
}

#[test]
fn atomic_rekeys_and_equal_keys_use_member_identity_as_deterministic_tie_breaker() {
    let store = Store::open_memory("membership-rekey").unwrap();
    create(&store);
    for member in [SEAT_A, SEAT_B, SEAT_C] { declare(&store, member, "true", member); }
    edit(&store, json!([place(SEAT_C, None, "a0"), place(SEAT_B, None, "a0"), place(SEAT_A, None, "a0")]));
    assert_eq!(members(&store), vec![SEAT_A, SEAT_B, SEAT_C]);
    let rekey = edit(&store, json!([place(SEAT_A, None, "a2"), place(SEAT_B, None, "a1"), place(SEAT_C, None, "a0")]));
    assert_eq!(members(&store), vec![SEAT_C, SEAT_B, SEAT_A]);
    assert!(rows(&store).iter().all(|row| row["revision"] == rekey.id));
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 3);
    let before = rows(&store);
    let index = store.index().unwrap();
    assert!(store.edit_ordered_memberships(&membership_input(json!([
        place(SEAT_A, None, "a3"), {"op":"remove","member":SEAT_A}
    ])), &BTreeMap::new()).is_err());
    assert!(store.edit_ordered_memberships(&membership_input(json!([
        place(SEAT_A, None, "a3"), place(SEAT_B, None, "invalid key")
    ])), &BTreeMap::new()).is_err());
    assert_eq!(store.index().unwrap(), index);
    assert_eq!(rows(&store), before);
}

#[test]
fn concurrent_pair_winners_converge_without_merging_bucket_and_key() {
    let a = Store::open_memory("membership-alder").unwrap();
    let b = Store::open_memory("membership-birch").unwrap();
    create(&a);
    declare(&a, SEAT_A, "true", "declare-a");
    folder_edit(&a, json!([{"op":"folder.create","id":FOLDER_A,"name":"A","parent":null,"key":"a0"}]));
    sync(&a, &b);
    let left = edit(&a, json!([place(SEAT_A, Some(FOLDER_A), "a1")]));
    let right = edit(&b, json!([place(SEAT_A, None, "a9")]));
    let mut envelopes = exchange_from(&a, &ReplicationInventory::default()).envelopes;
    envelopes.extend(exchange_from(&b, &ReplicationInventory::default()).envelopes);
    let c = Store::open_memory("membership-cedar").unwrap();
    let d = Store::open_memory("membership-dogwood").unwrap();
    for envelope in &envelopes { receive_and_project(&c, "relay", &exchange_of("relay", vec![envelope.clone()])); }
    for envelope in envelopes.iter().rev() { receive_and_project(&d, "relay", &exchange_of("relay", vec![envelope.clone()])); }
    let connection = c.readers.get();
    let expected = if canonical::claim_key(&connection, &left.id).unwrap() > canonical::claim_key(&connection, &right.id).unwrap() {
        (&left, json!({"bucket":FOLDER_A,"key":"a1"}))
    } else {
        (&right, json!({"bucket":null,"key":"a9"}))
    };
    drop(connection);
    assert_eq!(head(&c, SEAT_A).0, Some(expected.1));
    assert_eq!(head(&c, SEAT_A).1, expected.0.id);
    assert_eq!(head(&c, SEAT_A), head(&d, SEAT_A));
    assert_eq!(rows(&c), rows(&d));
    assert_eq!(membership_digests(&c), membership_digests(&d));
}

#[test]
fn deleted_folder_buckets_lift_to_live_ancestors_without_changing_raw_pair() {
    let store = Store::open_memory("membership-folders").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-a");
    folder_edit(&store, json!([
        {"op":"folder.create","id":FOLDER_A,"name":"A","parent":null,"key":"a0"},
        {"op":"folder.create","id":FOLDER_B,"name":"B","parent":FOLDER_A,"key":"a0"}
    ]));
    edit(&store, json!([place(SEAT_A, Some(FOLDER_B), "a7")]));
    let retained = head(&store, SEAT_A);
    let frontier = store.index().unwrap();
    folder_edit(&store, json!([{"op":"folder.delete","id":FOLDER_B}]));
    assert_eq!(rows(&store)[0]["position"], json!({"bucket":FOLDER_A,"key":"a7"}));
    assert_eq!(head(&store, SEAT_A), retained);
    assert!(store.ordered_memberships_changed(frontier, store.index().unwrap()).unwrap());
    folder_edit(&store, json!([{"op":"folder.delete","id":FOLDER_A}]));
    assert_eq!(rows(&store)[0]["position"], json!({"bucket":null,"key":"a7"}));
    assert_eq!(head(&store, SEAT_A), retained);
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 1);
    folder_edit(&store, json!([{"op":"retire"}]));
    assert!(rows(&store).is_empty());
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    assert_eq!(head(&store, SEAT_A), retained);
}

#[test]
fn version_one_placements_remain_legacy_and_cannot_be_implicitly_migrated() {
    let store = Store::open_memory("membership-v1").unwrap();
    folder_edit(&store, json!([{"op":"create","name":"Legacy"},
        {"op":"subject.place","subject":SEAT_A,"folder":null,"key":"a0"}]));
    let before = store.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
    assert_eq!(before["body"]["version"], 1);
    assert_eq!(before["body"]["placements"][SEAT_A]["value"]["key"], "a0");
    assert!(store.edit_ordered_memberships(&membership_input(json!([place(SEAT_A, None, "a1")])), &BTreeMap::new()).is_err());
    let mut conversion = arrangement_input(json!([{"op":"rename","name":"Converted"}]));
    conversion.fields.insert("version".into(), json!(2));
    assert!(store.edit_arrangement(&conversion, &BTreeMap::new()).is_err());
    assert_eq!(store.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), before);
    store.rebuild_claim_projections().unwrap();
    assert_eq!(store.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), before);
}

#[test]
fn concurrent_default_v1_and_explicit_v2_creates_preserve_legacy_in_both_orders() {
    for (legacy_origin, modern_origin, with_placement) in [
        ("collision-alder", "collision-zinnia", false),
        ("collision-zinnia", "collision-alder", false),
        ("collision-alder", "collision-zinnia", true),
        ("collision-zinnia", "collision-alder", true),
    ] {
        let legacy = Store::open_memory(legacy_origin).unwrap();
        let modern = Store::open_memory(modern_origin).unwrap();
        let mut legacy_operations = vec![json!({"op":"create","name":"Legacy"})];
        if with_placement { legacy_operations.push(json!({"op":"subject.place","subject":SEAT_A,"folder":null,"key":"a0"})); }
        folder_edit(&legacy, json!(legacy_operations));
        let version_rows: u64 = legacy.readers.get().query_row(
            "SELECT COUNT(*) FROM arrangement_registers WHERE register='version'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(version_rows, 0, "default-v1 projection must not gain a version register");
        create(&modern);
        declare(&modern, SEAT_A, "true", "collision-declare");
        let placed = edit(&modern, json!([place(SEAT_A, None, "a9")]));
        let mut envelopes = exchange_from(&legacy, &ReplicationInventory::default()).envelopes;
        envelopes.extend(exchange_from(&modern, &ReplicationInventory::default()).envelopes);
        let forward = Store::open_memory("collision-forward").unwrap();
        let reverse = Store::open_memory("collision-reverse").unwrap();
        for envelope in &envelopes { receive_and_project(&forward, "relay", &exchange_of("relay", vec![envelope.clone()])); }
        for envelope in envelopes.iter().rev() { receive_and_project(&reverse, "relay", &exchange_of("relay", vec![envelope.clone()])); }
        let expected = forward.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
        assert_eq!(expected["body"]["version"], 1);
        if with_placement {
            assert_eq!(expected["body"]["placements"][SEAT_A]["value"]["key"], "a0");
        } else { assert_eq!(expected["body"]["placements"], json!({})); }
        let digests = layout_digests(&forward);
        for target in [&forward, &reverse] {
            assert_eq!(target.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
            let read_error = target.ordered_memberships(CONTAINER, u64::MAX, None, 1000).unwrap_err();
            assert_eq!(read_error.downcast_ref::<St3Error>().unwrap().code, "invalid-arrangement-operations");
            assert_eq!(target.ordered_membership_count(CONTAINER).unwrap(), 0);
            assert_eq!(head(target, SEAT_A).1, placed.id, "v2 raw authority remains retained but inactive");
            assert_eq!(target.edit_ordered_memberships(
                &membership_input(json!([place(SEAT_A, None, "a1")])), &BTreeMap::new(),
            ).unwrap_err().code, "invalid-arrangement-operations");
            assert_eq!(layout_digests(target), digests);
            target.rebuild_claim_projections().unwrap();
            assert_eq!(target.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
            assert_eq!(layout_digests(target), digests);
        }
    }
}

#[test]
fn repaired_membership_original_retracts_omitted_pairs_and_shared_revision() {
    let source = Store::open_memory("membership-repair-source").unwrap();
    create(&source);
    declare(&source, SEAT_A, "true", "repair-declare-a");
    declare(&source, SEAT_B, "true", "repair-declare-b");
    let replacement = edit(&source, json!([place(SEAT_A, None, "a0")]));
    let original = edit(&source, json!([place(SEAT_A, None, "a9"), place(SEAT_B, None, "a1")]));
    assert_eq!(members(&source), vec![SEAT_B, SEAT_A]);
    assert_eq!(source.arrangement(CONTAINER, u64::MAX).unwrap().unwrap()["revision"], original.id);
    let receiver = Store::open_memory("membership-repair-receiver").unwrap();
    sync(&source, &receiver);
    repair(&receiver, &original, &replacement, "repair-membership-original");
    receive_and_project(&source, &receiver.origin, &exchange_from(&receiver, &source.replication_inventory().unwrap()));
    // A fresh receiver routes the original and repair in the same backlog. It must
    // never project an original already marked repaired by the authenticated repair.
    let fresh = Store::open_memory("membership-repair-fresh").unwrap();
    sync(&source, &fresh);
    let expected = source.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
    let digests = layout_digests(&source);
    assert_eq!(expected["revision"], replacement.id);
    for target in [&source, &receiver, &fresh] {
        assert_eq!(members(target), vec![SEAT_A]);
        assert_eq!(target.ordered_membership_count(CONTAINER).unwrap(), 1);
        assert_eq!(head(target, SEAT_A).0, Some(json!({"bucket":null,"key":"a0"})));
        assert_eq!(head(target, SEAT_A).1, replacement.id);
        assert_eq!(target.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
        let omitted: u64 = target.readers.get().query_row(
            "SELECT (SELECT COUNT(*) FROM ordered_membership_heads WHERE container=?1 AND member=?2)
                  + (SELECT COUNT(*) FROM declared_resource_edges WHERE owner=?1 AND relation='ordered-membership' AND target=?2)
                  + (SELECT COUNT(*) FROM ordered_membership_lifecycle WHERE subject=?2)",
            params![CONTAINER,SEAT_B], |row| row.get(0),
        ).unwrap();
        assert_eq!(omitted, 0, "repair must retract raw, reverse, and lifecycle dependencies for omitted pairs");
        assert_eq!(layout_digests(target), digests);
        target.rebuild_claim_projections().unwrap();
        assert_eq!(layout_digests(target), digests);
        assert!(target.replay_graph_for_heal().unwrap());
        assert_eq!(layout_digests(target), digests);
    }
    let directory = tempfile::tempdir().unwrap();
    let (plan, proof) = source.plan_checkpoint(now_ms() + 1, directory.path()).unwrap();
    assert!(proof.passed);
    source.apply_checkpoint_drop("checkpoint/repaired-memberships", &plan.envelopes, &plan.claims).unwrap();
    assert_eq!(layout_digests(&source), digests);
    // Receipt cleanup cannot resurrect repaired authority during a later rebuild.
    source.connection.write().execute("DELETE FROM replica_records WHERE claim_id=?1", [&original.id]).unwrap();
    source.rebuild_claim_projections().unwrap();
    assert_eq!(source.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
    assert_eq!(layout_digests(&source), digests);
}

#[test]
fn membership_repair_invalidates_the_receiver_frontier_even_after_rebuild_and_heal() {
    let source = Store::open_memory("membership-frontier-source").unwrap();
    create(&source);
    declare(&source, SEAT_A, "true", "frontier-declare-a");
    declare(&source, SEAT_B, "true", "frontier-declare-b");
    let replacement = edit(&source, json!([place(SEAT_A, None, "a0")]));
    let original = edit(&source, json!([place(SEAT_A, None, "a9"), place(SEAT_B, None, "a1")]));
    let receiver = Store::open_memory("membership-frontier-receiver").unwrap();
    sync(&source, &receiver);
    let before = receiver.index().unwrap();
    assert_eq!(members(&receiver), vec![SEAT_B, SEAT_A]);
    repair(&receiver, &original, &replacement, "repair-membership-frontier");
    let repaired = receiver.index().unwrap();
    assert!(repaired > before);
    assert!(receiver.ordered_memberships_changed(before, repaired).unwrap());
    let changed: u64 = receiver.readers.get().query_row(
        "SELECT changed_index FROM ordered_membership_counts WHERE container=?1",
        [CONTAINER], |row| row.get(0),
    ).unwrap();
    assert_eq!(changed, repaired, "repair must invalidate at its own frontier, not the retained operation's index");
    assert!(receiver.ordered_memberships(CONTAINER, before, None, 1000).is_err());
    assert_eq!(members(&receiver), vec![SEAT_A]);
    let expected = receiver.ordered_memberships(CONTAINER, repaired, None, 1000).unwrap();
    let digests = layout_digests(&receiver);
    for heal in [false, true] {
        if heal {
            assert!(receiver.replay_graph_for_heal().unwrap());
        } else {
            receiver.rebuild_claim_projections().unwrap();
        }
        assert_eq!(layout_digests(&receiver), digests, "local invalidation is not shared projection authority");
        assert!(receiver.ordered_memberships_changed(before, repaired).unwrap());
        assert!(receiver.ordered_memberships(CONTAINER, before, None, 1000).is_err());
        assert_eq!(receiver.ordered_memberships(CONTAINER, repaired, None, 1000).unwrap(), expected);
    }
}

#[test]
fn creation_repair_invalidates_even_when_the_membership_count_row_disappears() {
    let source = Store::open_memory("creation-frontier-source").unwrap();
    create(&source);
    let original = source.claims_for(CONTAINER, Some("arrangement.edited")).unwrap().remove(0);
    let replacement = folder_edit(&source, json!([{"op":"rename","name":"Retained name"}]));
    let receiver = Store::open_memory("creation-frontier-receiver").unwrap();
    sync(&source, &receiver);
    let before = receiver.index().unwrap();
    assert!(receiver.ordered_memberships(CONTAINER, before, None, 1000).unwrap().is_empty());
    repair(&receiver, &original, &replacement, "repair-creation-frontier");
    let repaired = receiver.index().unwrap();
    let counts: u64 = receiver.readers.get().query_row(
        "SELECT COUNT(*) FROM ordered_membership_counts WHERE container=?1",
        [CONTAINER], |row| row.get(0),
    ).unwrap();
    assert_eq!(counts, 0, "retracting version-two creation removes its count row");
    let digests = layout_digests(&receiver);
    for heal in [None, Some(false), Some(true)] {
        match heal {
            None => {}
            Some(false) => receiver.rebuild_claim_projections().unwrap(),
            Some(true) => { assert!(receiver.replay_graph_for_heal().unwrap()); }
        }
        assert!(receiver.ordered_memberships_changed(before, repaired).unwrap());
        assert!(receiver.arrangements_changed(before, repaired).unwrap());
        assert!(receiver.arrangement(CONTAINER, before).is_err());
        assert!(receiver.arrangements("person/ada", before).is_err());
        assert!(receiver.arrangement(CONTAINER, repaired).unwrap().is_none());
        assert!(receiver.ordered_memberships(CONTAINER, before, None, 1000).is_err());
        // The surviving rename does not declare a container after its create is repaired.
        // Strict reads must refuse the unknown ID, not return an empty membership page.
        let error = receiver.ordered_memberships(CONTAINER, repaired, None, 1000).unwrap_err();
        assert_eq!(error.downcast_ref::<St3Error>().unwrap().code, "not-found");
        assert_eq!(layout_digests(&receiver), digests);
    }
}

#[test]
fn retained_claim_member_repair_refreshes_lifecycle_without_changing_its_pair() {
    let source = Store::open_memory("retained-member-source").unwrap();
    create(&source);
    let member = "attention/membership-repair";
    let replacement = source.append_claim(&claim(member, "attention.requested", json!({
        "reviewer":"person/ada","title":"Retained request","reason":"Review the retained request",
        "severity":"warning","targets":[]
    }))).unwrap();
    let original = source.append_claim(&claim(member, "attention.resolved", json!({
        "request":replacement.id,"outcome":"resolved","reason":"Resolved by the selected newer claim"
    }))).unwrap();
    edit(&source, json!([place(member, None, "a0")]));
    let receiver = Store::open_memory("retained-member-receiver").unwrap();
    sync(&source, &receiver);
    let retained = head(&receiver, member);
    assert_eq!(lifecycle(&receiver, member), (true, original.id.clone()));
    let before = receiver.index().unwrap();
    repair(&receiver, &original, &replacement, "repair-retained-member");
    let repaired = receiver.index().unwrap();
    assert_eq!(lifecycle(&receiver, member), (true, replacement.id.clone()));
    assert_ne!(lifecycle(&receiver, member).1, original.id);
    assert_eq!(head(&receiver, member), retained);
    assert_eq!(members(&receiver), vec![member]);
    assert!(receiver.ordered_memberships_changed(before, repaired).unwrap());
    assert!(receiver.ordered_memberships(CONTAINER, before, None, 1000).is_err());
    let expected = rows(&receiver);
    let digests = membership_digests(&receiver);
    receive_and_project(&source, &receiver.origin, &exchange_from(&receiver, &source.replication_inventory().unwrap()));
    let fresh = Store::open_memory("retained-member-fresh").unwrap();
    sync(&receiver, &fresh);
    for target in [&receiver, &source, &fresh] {
        assert_eq!(lifecycle(target, member), (true, replacement.id.clone()));
        assert_eq!(head(target, member), retained);
        assert_eq!(rows(target), expected);
        assert_eq!(membership_digests(target), digests);
        target.rebuild_claim_projections().unwrap();
        assert_eq!(membership_digests(target), digests);
        assert_eq!(lifecycle(target, member), (true, replacement.id.clone()));
        assert!(target.replay_graph_for_heal().unwrap());
        assert_eq!(membership_digests(target), digests);
        assert_eq!(head(target, member), retained);
    }
    let directory = tempfile::tempdir().unwrap();
    let (plan, proof) = receiver.plan_checkpoint(now_ms() + 1, directory.path()).unwrap();
    assert!(proof.passed, "checkpoint replay witness must agree with incremental lifecycle repair");
    receiver.apply_checkpoint_drop("checkpoint/retained-member-repair", &plan.envelopes, &plan.claims).unwrap();
    assert_eq!(membership_digests(&receiver), digests);
    assert_eq!(lifecycle(&receiver, member), (true, replacement.id));
    assert_eq!(head(&receiver, member), retained);
}

#[test]
fn repairing_legacy_creation_releases_only_unrepaired_v2_authority() {
    let legacy = Store::open_memory("legacy-repair-source").unwrap();
    let original = folder_edit(&legacy, json!([{"op":"create","name":"Legacy"}]));
    let legacy_placement = folder_edit(&legacy, json!([
        {"op":"subject.place","subject":SEAT_A,"folder":null,"key":"a0"},
    ]));
    let modern = Store::open_memory("modern-repair-source").unwrap();
    create(&modern);
    let replacement = modern.claims_for(CONTAINER, Some("arrangement.edited")).unwrap().remove(0);
    declare(&modern, SEAT_A, "true", "modern-repair-declare");
    edit(&modern, json!([place(SEAT_A, None, "a9")]));
    let receiver = Store::open_memory("legacy-repair-receiver").unwrap();
    sync(&modern, &receiver);
    sync(&legacy, &receiver);
    assert_eq!(receiver.arrangement(CONTAINER, u64::MAX).unwrap().unwrap()["body"]["version"], 1);
    let read_error = receiver.ordered_memberships(CONTAINER, u64::MAX, None, 1000).unwrap_err();
    assert_eq!(read_error.downcast_ref::<St3Error>().unwrap().code, "invalid-arrangement-operations");
    assert_eq!(receiver.ordered_membership_count(CONTAINER).unwrap(), 0);
    repair(&receiver, &original, &replacement, "repair-legacy-creation");
    let still_legacy = receiver.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
    assert_eq!(still_legacy["body"]["version"], 1, "unrepaired placements independently prevent implicit migration");
    assert_eq!(still_legacy["body"]["placements"][SEAT_A]["value"]["key"], "a0");
    let read_error = receiver.ordered_memberships(CONTAINER, u64::MAX, None, 1000).unwrap_err();
    assert_eq!(read_error.downcast_ref::<St3Error>().unwrap().code, "invalid-arrangement-operations");
    assert_eq!(receiver.ordered_membership_count(CONTAINER).unwrap(), 0);
    repair(&receiver, &legacy_placement, &replacement, "repair-legacy-placement");
    let expected = receiver.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
    assert_eq!(expected["body"]["version"], 2);
    assert!(expected["body"].get("placements").is_none());
    assert_eq!(members(&receiver), vec![SEAT_A]);
    let placement_rows: u64 = receiver.readers.get().query_row(
        "SELECT COUNT(*) FROM arrangement_registers WHERE subject=?1 AND register LIKE 'placement/%'",
        [CONTAINER], |row| row.get(0),
    ).unwrap();
    assert_eq!(placement_rows, 0);
    let digests = layout_digests(&receiver);
    let fresh = Store::open_memory("legacy-repair-fresh").unwrap();
    sync(&receiver, &fresh);
    assert_eq!(fresh.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
    assert_eq!(layout_digests(&fresh), digests);
    receiver.rebuild_claim_projections().unwrap();
    assert_eq!(receiver.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), expected);
    assert_eq!(layout_digests(&receiver), digests);
}

#[test]
fn direct_mission_retirement_flushes_membership_visibility_in_the_same_transaction() {
    let store = Store::open_memory("membership-mission-retire").unwrap();
    create(&store);
    publish(&store, "version 2\nmission \"membership-retire\" state=\"ready\" { goal \"Retain the position after retirement.\" }\n".into(),
        "publish-membership-mission");
    let member = "mission/membership-retire";
    edit(&store, json!([place(member, None, "a0")]));
    let retained = head(&store, member);
    assert_eq!(members(&store), vec![member]);
    store.retire_mission(member, "person/ada", "retire-membership-mission").unwrap();
    assert!(rows(&store).is_empty());
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    assert_eq!(head(&store, member), retained);
    assert!(!lifecycle(&store, member).0);
    let pending: u64 = store.readers.get().query_row(
        "SELECT COUNT(*) FROM local_ordered_membership_pending", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(pending, 0);
}

#[test]
fn version_two_is_folder_only_and_version_can_only_be_supplied_on_creation() {
    let store = Store::open_memory("membership-v2").unwrap();
    create(&store);
    let before = store.arrangement(CONTAINER, u64::MAX).unwrap().unwrap();
    assert_eq!(before["body"]["version"], 2);
    assert!(store.edit_arrangement(&arrangement_input(json!([
        {"op":"subject.place","subject":SEAT_A,"folder":null,"key":"a0"}
    ])), &BTreeMap::new()).is_err());
    let mut edit_version = arrangement_input(json!([{"op":"rename","name":"Changed"}]));
    edit_version.fields.insert("version".into(), json!(2));
    assert!(store.edit_arrangement(&edit_version, &BTreeMap::new()).is_err());
    assert_eq!(store.arrangement(CONTAINER, u64::MAX).unwrap().unwrap(), before);
}

#[test]
fn membership_admission_reuses_actor_fences_and_idempotency_receipts() {
    let store = Store::open_memory("membership-admission").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-a");
    let operations = json!([place(SEAT_A, None, "a0")]);
    for actor in [None, Some("person/other"), Some("daemon/runtime")] {
        let mut input = membership_input(operations.clone());
        input.actor = actor.map(str::to_owned);
        assert!(store.edit_ordered_memberships(&input, &BTreeMap::new()).is_err());
    }
    let mut wrong_owner = membership_input(operations.clone());
    wrong_owner.fields.insert("owner".into(), json!("person/other"));
    assert!(store.edit_ordered_memberships(&wrong_owner, &BTreeMap::new()).is_err());
    let authority = store.append_claim(&claim("resource/membership-authority", "resource.observed", json!({"kind":"custom.test.membership"}))).unwrap();
    let mut input = membership_input(operations);
    input.actor = Some("agent/fleet/fixture-memberships/seat".into());
    input.idempotency_key = Some("membership-action".into());
    input.fields.insert("action_id".into(), json!("membership-action"));
    input.fields.insert("action_digest".into(), json!("0".repeat(64)));
    let mut fences = BTreeMap::from([(authority.subject.clone(), "stale".into())]);
    assert!(store.edit_ordered_memberships(&input, &fences).is_err());
    assert!(rows(&store).is_empty());
    fences.insert(authority.subject.clone(), authority.id);
    let accepted = store.edit_ordered_memberships(&input, &fences).unwrap();
    assert_eq!(accepted.actor, input.actor);
    fences.insert(authority.subject, "stale-again".into());
    assert_eq!(store.edit_ordered_memberships(&input, &fences).unwrap().id, accepted.id);
    input.fields.insert("operations".into(), json!([place(SEAT_A, None, "a1")]));
    input.fields.insert("action_digest".into(), json!("1".repeat(64)));
    assert!(store.edit_ordered_memberships(&input, &fences).is_err());
    assert_eq!(rows(&store)[0]["revision"], accepted.id);
}

#[test]
fn keyset_small_windows_skip_many_hidden_pairs_and_seek_the_live_order_index() {
    let store = Store::open_memory("membership-pages").unwrap();
    create(&store);
    let hidden_count = super::ordered_membership::MAX_LIVE_MEMBERS + 1;
    for start in (0..hidden_count).step_by(512) {
        let operations: Vec<_> = (start..(start + 512).min(hidden_count))
            .map(|index| place(&format!("agent/unknown/seat-{index:05}"), None, "a0")).collect();
        edit(&store, json!(operations));
    }
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 0);
    for member in [SEAT_A, SEAT_B, SEAT_C] { declare(&store, member, "true", member); }
    folder_edit(&store, json!([{"op":"folder.create","id":FOLDER_A,"name":"A","parent":null,"key":"a0"}]));
    edit(&store, json!([place(SEAT_B, None, "a1"), place(SEAT_A, None, "a1"), place(SEAT_C, Some(FOLDER_A), "a0")]));
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 3);
    let through = store.index().unwrap();
    let mut after: Option<(String, String, String)> = None;
    let mut paged = Vec::new();
    loop {
        let page = store.ordered_memberships(CONTAINER, through,
            after.as_ref().map(|(bucket, key, member)| (bucket.as_str(), key.as_str(), member.as_str())), 1).unwrap();
        assert!(page.len() <= 1);
        let Some(row) = page.first() else { break; };
        after = Some((
            row["position"]["bucket"].as_str().unwrap_or("").to_owned(),
            row["position"]["key"].as_str().unwrap().to_owned(),
            row["member"].as_str().unwrap().to_owned(),
        ));
        paged.push(row.clone());
        assert!(paged.len() <= 3, "a keyset cursor must advance even for equal keys");
    }
    assert_eq!(paged, rows(&store));
    assert_eq!(members(&store), vec![SEAT_A, SEAT_B, SEAT_C]);
    let connection = store.readers.get();
    let heads: u64 = connection.query_row("SELECT COUNT(*) FROM ordered_membership_heads WHERE container=?1",
        [CONTAINER], |row| row.get(0)).unwrap();
    assert_eq!(heads, u64::try_from(hidden_count + 3).unwrap());
    let details = connection.prepare(
        "EXPLAIN QUERY PLAN SELECT bucket,key,member,revision FROM ordered_membership_live
         WHERE container=?1 AND (bucket,key,member)>(?2,?3,?4)
         ORDER BY bucket,key,member LIMIT ?5"
    ).unwrap().query_map(params![CONTAINER, "", "a1", SEAT_A, 1], |row| row.get::<_, String>(3))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(details.iter().any(|detail| detail.contains("SEARCH ordered_membership_live")), "{details:?}");
    assert!(details.iter().all(|detail| !detail.contains("TEMP B-TREE") && !detail.contains("SCAN ordered_membership_heads")), "{details:?}");
    drop(connection);
    retire(&store, SEAT_A, "retire-page-a");
    assert!(store.ordered_memberships(CONTAINER, through, None, 1).is_err(), "old frontiers must not silently read the new projection");
    assert_eq!(store.ordered_membership_count(CONTAINER).unwrap(), 2);
}

#[test]
fn local_admission_counts_live_pairs_and_replication_never_truncates_at_the_limit() {
    let a = Store::open_memory("membership-limit-a").unwrap();
    let b = Store::open_memory("membership-limit-b").unwrap();
    create(&a);
    let limit = super::ordered_membership::MAX_LIVE_MEMBERS;
    let mut kdl = String::from("version 2\n");
    for index in 0..=limit {
        kdl.push_str(&format!("agent \"limit/seat-{index:05}\" {{ workspace \"/tmp\"; command \"true\"; }}\n"));
    }
    publish(&a, kdl, "declare-limit-seats");
    sync(&a, &b);
    for start in (0..limit).step_by(512) {
        let operations: Vec<_> = (start..(start + 512).min(limit))
            .map(|index| place(&format!("agent/limit/seat-{index:05}"), None, "a0")).collect();
        edit(&a, json!(operations));
    }
    assert_eq!(a.ordered_membership_count(CONTAINER).unwrap(), u64::try_from(limit).unwrap());
    edit(&a, json!([place("agent/unknown/at-limit", None, "a0")]));
    assert_eq!(a.ordered_membership_count(CONTAINER).unwrap(), u64::try_from(limit).unwrap());
    let overflow = format!("agent/limit/seat-{limit:05}");
    let index = a.index().unwrap();
    assert!(a.edit_ordered_memberships(&membership_input(json!([place(&overflow, None, "a0")])), &BTreeMap::new()).is_err());
    assert_eq!(a.index().unwrap(), index);
    edit(&b, json!([place(&overflow, None, "a0")]));
    sync(&a, &b);
    sync(&b, &a);
    let converged_count = u64::try_from(limit + 1).unwrap();
    assert_eq!(a.ordered_membership_count(CONTAINER).unwrap(), converged_count);
    assert_eq!(b.ordered_membership_count(CONTAINER).unwrap(), converged_count);
    assert_eq!(membership_digests(&a), membership_digests(&b));
    let first = "agent/limit/seat-00000";
    let retained = head(&a, first);
    retire(&a, first, "retire-limit-first");
    retire(&a, &overflow, "retire-limit-overflow");
    assert_eq!(a.ordered_membership_count(CONTAINER).unwrap(), u64::try_from(limit - 1).unwrap());
    declare(&a, SEAT_A, "true", "declare-replacement-seat");
    edit(&a, json!([place(SEAT_A, None, "a1")]));
    assert_eq!(a.ordered_membership_count(CONTAINER).unwrap(), u64::try_from(limit).unwrap());
    assert_eq!(head(&a, first), retained);
}

#[test]
fn named_resource_replacement_preserves_membership_and_reverse_relations_do_not_collide() {
    let store = Store::open_memory("membership-reverse").unwrap();
    create(&store);
    let publish_resources = |resources: &str, command: &str, key: &str| {
        publish(&store, format!("version 2\nagent \"ada/seat-a\" {{ workspace \"/tmp\"; command {command:?}; {resources} }}\n"), key);
    };
    publish_resources("resource \"goal\" subject=\"resource/shared\" reason=\"read only\";", "true", "resources-a");
    edit(&store, json!([place(SEAT_A, None, "a0")]));
    let retained = head(&store, SEAT_A);
    assert_eq!(store.declared_resource_referrers("resource/shared").unwrap(), vec![json!({"owner":SEAT_A,"name":"goal","reason":"read only"})]);
    assert!(store.declared_resource_referrers(SEAT_A).unwrap().is_empty(), "membership edges are not named declaration references");
    publish_resources("resource \"replacement\" subject=\"resource/other\";", "echo replacement", "resources-b");
    assert!(store.declared_resource_referrers("resource/shared").unwrap().is_empty());
    assert_eq!(members(&store), vec![SEAT_A]);
    assert_eq!(head(&store, SEAT_A), retained);
    publish_resources("", "echo no-resources", "resources-c");
    assert!(store.declared_resource_referrers("resource/other").unwrap().is_empty());
    assert_eq!(members(&store), vec![SEAT_A]);
    let connection = store.readers.get();
    let (name, target): (String, String) = connection.query_row(
        "SELECT name,target FROM declared_resource_edges WHERE owner=?1 AND relation='ordered-membership'",
        [CONTAINER], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(target, SEAT_A);
    assert_eq!(name, SEAT_A);
    // Arrangements cannot author named resources today, so inspect the actual composite key
    // rather than manufacture a declaration outside the public schema.
    let primary_key = connection.prepare(
        "SELECT name FROM pragma_table_info('declared_resource_edges') WHERE pk>0 ORDER BY pk"
    ).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert_eq!(primary_key, vec!["owner", "relation", "name"]);
    let details = connection.prepare("EXPLAIN QUERY PLAN SELECT owner,relation,name FROM declared_resource_edges WHERE target=?1")
        .unwrap().query_map([SEAT_A], |row| row.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    assert!(details.iter().any(|detail| detail.contains("declared_resource_edges_target")), "{details:?}");
}

#[test]
fn replay_rebuild_replication_and_checkpoint_retain_raw_hidden_and_absent_authority() {
    let store = Store::open_memory("membership-checkpoint").unwrap();
    create(&store);
    declare(&store, SEAT_A, "true", "declare-a");
    declare(&store, SEAT_B, "true", "declare-b");
    let mut first_input = membership_input(json!([place(SEAT_A, None, "a0"), place(SEAT_B, None, "a1"), place(SEAT_C, None, "a2")]));
    first_input.actor = Some("agent/fleet/fixture-memberships/seat".into());
    let first = store.edit_ordered_memberships(&first_input, &BTreeMap::new()).unwrap();
    let mut remove_input = membership_input(json!([{"op":"remove","member":SEAT_A}]));
    remove_input.actor = first_input.actor.clone();
    let removed = store.edit_ordered_memberships(&remove_input, &BTreeMap::new()).unwrap();
    retire(&store, SEAT_B, "retire-b");
    for number in 0..5 {
        let mut noise = claim("daemon/membership-noise", "daemon.diagnostic",
            json!({"severity":"warning","code":"membership-checkpoint-noise","reason":format!("sample {number}")}));
        noise.actor = None;
        store.append_claim(&noise).unwrap();
    }
    let retained: Vec<_> = [SEAT_A, SEAT_B, SEAT_C].iter().map(|member| head(&store, member)).collect();
    let before = rows(&store);
    assert!(before.is_empty());
    let digests = membership_digests(&store);
    let replica = Store::open_memory("membership-replica").unwrap();
    sync(&store, &replica);
    assert_eq!(membership_digests(&replica), digests);
    for target in [&store, &replica] {
        target.rebuild_claim_projections().unwrap();
        assert_eq!(rows(target), before);
        assert_eq!(membership_digests(target), digests);
        assert!(target.replay_graph_for_heal().unwrap());
        assert_eq!(membership_digests(target), digests);
        assert_eq!(target.ordered_membership_count(CONTAINER).unwrap(), 0);
        for (member, expected) in [SEAT_A, SEAT_B, SEAT_C].iter().zip(&retained) {
            assert_eq!(&head(target, member), expected);
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (plan, proof) = store.plan_checkpoint(now_ms() + 1, directory.path()).unwrap();
    assert!(proof.passed);
    assert!(plan.claims.iter().any(|claim| claim.kind == "daemon.diagnostic"), "the checkpoint must actually trim unrelated history");
    assert!(plan.claims.iter().all(|claim| claim.kind != "ordered-membership.edited" && claim.kind != "arrangement.edited"));
    let authority = store.replication_inventory().unwrap().digest;
    store.apply_checkpoint_drop("checkpoint/memberships", &plan.envelopes, &plan.claims).unwrap();
    assert_eq!(store.replication_inventory().unwrap().digest, authority);
    assert_eq!(membership_digests(&store), digests);
    assert!(store.claim_by_id(&first.id).unwrap().is_some());
    assert!(store.claim_by_id(&removed.id).unwrap().is_some());
    store.rebuild_claim_projections().unwrap();
    assert_eq!(membership_digests(&store), digests);
    declare(&store, SEAT_B, "echo restored", "redeclare-b");
    declare(&store, SEAT_C, "true", "declare-c");
    assert_eq!(members(&store), vec![SEAT_B, SEAT_C]);
    assert_eq!(head(&store, SEAT_A), retained[0]);
    assert_eq!(head(&store, SEAT_B), retained[1]);
    assert_eq!(head(&store, SEAT_C), retained[2]);
}
