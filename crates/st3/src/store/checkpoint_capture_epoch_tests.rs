//! Capture invalidation follows the projection/trim APIs, not just their current callers.
use super::*;

fn epoch(store: &Store) -> i64 {
    smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap()
}

fn publish(store: &Store, source: &str, key: &str) {
    let intent = crate::graph::parse_test_intent(source, &store.origin).unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store.apply(&intent, &preview.subject_tokens, key).unwrap();
}

fn projected_store() -> Store {
    let store = Store::open_memory("writer").unwrap();
    assert_eq!(
        store
            .readers
            .get()
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        st3_schema::STORAGE_VERSION,
        "the storage registry must name the initialized SQLite version"
    );
    publish(
        &store,
        "version 2\nagent \"capture\" { workspace \"/tmp\"; command \"true\" }\n",
        "capture-declaration",
    );
    publish(
        &store,
        "version 2\nmission \"capture\" state=\"ready\" { goal \"Preserve captured metadata.\"; step \"work\" { agentless } }\n",
        "capture-mission",
    );
    store
}

#[test]
fn desired_projection_rebuild_advances_capture_epoch() {
    let store = projected_store();
    let subject = "agent/writer.capture";
    let before = epoch(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    rebuild_base_aggregate_tx(&tx, &Aggregate::Desired(subject.into())).unwrap();
    assert!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap() > before);
    tx.commit().unwrap();
    drop(writer);
    assert!(epoch(&store) > before);
    assert_eq!(store.readers.get().query_row(
        "SELECT COUNT(*) FROM desired WHERE subject=?1", [subject], |row| row.get::<_, u64>(0),
    ).unwrap(), 1);
}

#[test]
fn mission_definitions_projection_rebuild_advances_capture_epoch() {
    let store = projected_store();
    let before = epoch(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    rebuild_base_aggregate_tx(&tx, &Aggregate::Mission("mission/capture".into())).unwrap();
    assert!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap() > before);
    tx.commit().unwrap();
    drop(writer);
    assert!(epoch(&store) > before);
    assert_eq!(store.readers.get().query_row(
        "SELECT COUNT(*) FROM mission_definitions WHERE mission_id='capture'", [], |row| row.get::<_, u64>(0),
    ).unwrap(), 1);
}

#[test]
fn direct_desired_deletion_advances_capture_epoch_without_a_claim_append() {
    let store = projected_store();
    store.connection.write().execute(
        "UPDATE desired SET owner_run='mission-run/capture-owner' WHERE subject='agent/writer.capture'", [],
    ).unwrap();
    let index = store.index().unwrap();
    let before = epoch(&store);
    assert_eq!(store.discard_desired_owned_by("mission-run/capture-owner").unwrap(), 1);
    assert!(epoch(&store) > before);
    assert_eq!(store.index().unwrap(), index, "deletion unexpectedly appended a claim");
    assert_eq!(store.readers.get().query_row(
        "SELECT COUNT(*) FROM desired WHERE subject='agent/writer.capture'", [], |row| row.get::<_, u64>(0),
    ).unwrap(), 0);
}

#[test]
fn all_projection_protection_insert_update_delete_paths_advance_capture_epoch() {
    let store = projected_store();
    for (table, key, copy) in [
        ("desired", "subject", "INSERT INTO desired(subject,kind,revision,claim_id,body) SELECT 'agent/writer.copy',kind,revision,claim_id,body FROM desired WHERE subject='agent/writer.capture'"),
        ("mission_definitions", "mission_id", "INSERT INTO mission_definitions(mission_id,revision,state,claim_id) SELECT 'copy',revision,state,claim_id FROM mission_definitions WHERE mission_id='capture'"),
        ("mission_revisions", "mission_id", "INSERT INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index) SELECT 'copy',revision,state,body,claim_id,created_index FROM mission_revisions WHERE mission_id='capture'"),
    ] {
        let mut writer = store.connection.write();
        let before = smallclaims::store::checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        assert!(tx.execute(copy, []).unwrap() > 0, "{table} insert fixture was empty");
        let inserted = smallclaims::store::checkpoint_capture_epoch(&tx).unwrap();
        assert!(inserted > before, "{table} insert");
        assert!(tx.execute(&format!("UPDATE {table} SET {key}={key}"), []).unwrap() > 0);
        let updated = smallclaims::store::checkpoint_capture_epoch(&tx).unwrap();
        assert!(updated > inserted, "{table} update");
        assert!(tx.execute(&format!("DELETE FROM {table}"), []).unwrap() > 0);
        assert!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap() > updated, "{table} delete");
        tx.rollback().unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}

#[test]
fn checkpoint_trim_advances_capture_epoch_with_tombstones_and_deletions() {
    let store = Store::open_memory("writer").unwrap();
    for n in 0..5 {
        store.append_claim(&ClaimInput {
            subject: "daemon/writer".into(),
            kind: "daemon.diagnostic".into(),
            actor: None,
            fields: BTreeMap::from([
                ("severity".into(), json!("warning")),
                ("code".into(), json!("capture-test")),
                ("reason".into(), json!(format!("diagnostic {n}"))),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("capture-diagnostic-{n}")),
        }).unwrap();
    }
    let sealed = store.checkpoint_sealed_set(now_ms() + 1_000).unwrap();
    let plan = checkpoint_rules::plan_drops(&sealed);
    assert!(!plan.claims.is_empty(), "trim fixture had no drops");
    let before = epoch(&store);
    store.apply_checkpoint_drop("checkpoint/test", &plan.envelopes, &plan.claims).unwrap();
    assert!(epoch(&store) > before);
    for claim in &plan.claims {
        assert!(store.claim_by_id(&claim.id).unwrap().is_none());
    }
    assert_eq!(store.checkpointed_envelopes().unwrap(), plan.envelopes.len() as u64);
}
