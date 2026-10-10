//! Capture invalidation follows the projection/trim APIs, not just their current callers.
use super::*;

fn epoch(store: &Store) -> i64 {
    smallclaims::store::checkpoint_capture_epoch(&store.readers.get()).unwrap()
}

fn capture_bounds(store: &Store) -> i64 {
    store.seal_local_batches().unwrap();
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let frontier = smallclaims::store::max_envelope_rowid(&tx).unwrap();
    let cut = i64::try_from(now_ms() + 1_000).unwrap();
    tx.execute(
        "UPDATE checkpoint_capture_epoch
         SET envelope_frontier=MAX(envelope_frontier,?1),cut_unix_ms=MAX(cut_unix_ms,?2)
         WHERE id=1 AND (envelope_frontier<?1 OR cut_unix_ms<?2)",
        [frontier, cut],
    ).unwrap();
    tx.commit().unwrap();
    cut
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
    capture_bounds(&store);
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
    for (table, copy) in [
        ("desired", "INSERT INTO desired(subject,kind,revision,claim_id,body) SELECT 'agent/writer.copy',kind,revision,claim_id,body FROM desired WHERE subject='agent/writer.capture'"),
        ("mission_definitions", "INSERT INTO mission_definitions(mission_id,revision,state,claim_id) SELECT 'copy',revision,state,claim_id FROM mission_definitions WHERE mission_id='capture'"),
        ("mission_revisions", "INSERT INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index) SELECT 'copy',revision,state,body,claim_id,created_index FROM mission_revisions WHERE mission_id='capture'"),
    ] {
        let mut writer = store.connection.write();
        let before = smallclaims::store::checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        assert!(tx.execute(copy, []).unwrap() > 0, "{table} insert fixture was empty");
        let inserted = smallclaims::store::checkpoint_capture_epoch(&tx).unwrap();
        assert!(inserted > before, "{table} insert");
        assert!(tx.execute(
            &format!("UPDATE {table} SET claim_id=(SELECT id FROM claims WHERE id<>{table}.claim_id LIMIT 1)"),
            [],
        ).unwrap() > 0);
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

#[test]
fn protection_updates_fence_old_and_new_below_cut_references_but_ignore_newer_targets() {
    let store = projected_store();
    let cut: i64 = store.readers.get().query_row(
        "SELECT cut_unix_ms FROM checkpoint_capture_epoch WHERE id=1", [], |row| row.get(0),
    ).unwrap();
    let captured: String = store.readers.get().query_row(
        "SELECT claim_id FROM desired WHERE subject='agent/writer.capture'", [], |row| row.get(0),
    ).unwrap();
    let newer = store.append_claim(&ClaimInput {
        subject: "daemon/writer".into(),
        kind: "daemon.diagnostic".into(),
        actor: None,
        fields: BTreeMap::from([
            ("severity".into(), json!("warning")),
            ("code".into(), json!("newer-protection")),
            ("reason".into(), json!("above-cut protection fixture")),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some("newer-protection".into()),
    }).unwrap();
    {
        let writer = store.connection.write();
        writer.execute(
            "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
            params![newer.id, (cut + 100).to_string()],
        ).unwrap();
        writer.execute(
            "UPDATE batches SET accepted_at_unix_ms=?2 WHERE id=?1",
            params![newer.batch_id, (cut + 100).to_string()],
        ).unwrap();
    }
    store.seal_local_batches().unwrap();
    let mut writer = store.connection.write();
    writer.execute("INSERT INTO blobs(hash,bytes,size) VALUES('protection-blob',x'',0)", []).unwrap();
    for (table, insert, reference) in [
        ("desired", "INSERT INTO desired(subject,kind,revision,claim_id,body) VALUES('agent/writer.newer','agent','newer',?1,'{}')", "claim_id"),
        ("mission_definitions", "INSERT INTO mission_definitions(mission_id,revision,state,claim_id) VALUES('newer','newer','ready',?1)", "claim_id"),
        ("mission_revisions", "INSERT INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index) VALUES('newer','newer','ready','{}',?1,1)", "claim_id"),
        ("documents", "INSERT INTO documents(name,hash,created_index,binding_claim_id) VALUES('newer','protection-blob',1,?1)", "binding_claim_id"),
    ] {
        let before = smallclaims::store::checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(insert, [&newer.id]).unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap(), before, "{table} above-cut insert");
        tx.execute(
            &format!("UPDATE {table} SET {reference}={reference} WHERE {reference}=?1"), [&newer.id],
        ).unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap(), before, "{table} no-op");
        tx.execute(
            &format!("UPDATE {table} SET {reference}=?1 WHERE {reference}=?2"),
            params![captured, newer.id],
        ).unwrap();
        let protected = smallclaims::store::checkpoint_capture_epoch(&tx).unwrap();
        assert!(protected > before, "{table} NEW captured reference");
        tx.execute(
            &format!("UPDATE {table} SET {reference}=?1 WHERE {reference}=?2"),
            params![newer.id, captured],
        ).unwrap();
        assert!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap() > protected, "{table} OLD captured reference");
        let updated = smallclaims::store::checkpoint_capture_epoch(&tx).unwrap();
        tx.execute(
            &format!("DELETE FROM {table} WHERE {reference}=?1"), [&newer.id],
        ).unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap(), updated, "{table} above-cut delete");
        tx.rollback().unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}

#[test]
fn protection_replace_cannot_remove_captured_reference_when_recursive_triggers_are_off() {
    let store = projected_store();
    let mut writer = store.connection.write();
    writer.pragma_update(None, "recursive_triggers", false).unwrap();
    for sql in [
        "INSERT OR REPLACE INTO desired(subject,kind,revision,claim_id,body)
         SELECT subject,kind,revision,'uncaptured',body FROM desired",
        "INSERT OR REPLACE INTO mission_definitions(mission_id,revision,state,claim_id)
         SELECT mission_id,revision,state,'uncaptured' FROM mission_definitions",
        "INSERT OR REPLACE INTO mission_revisions(mission_id,revision,state,body,claim_id,created_index)
         SELECT mission_id,revision,state,body,'uncaptured',created_index FROM mission_revisions",
    ] {
        writer.pragma_update(None, "foreign_keys", false).unwrap();
        let before = smallclaims::store::checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        assert!(tx.execute(sql, []).unwrap() > 0);
        assert!(smallclaims::store::checkpoint_capture_epoch(&tx).unwrap() > before, "{sql}");
        tx.rollback().unwrap();
        writer.pragma_update(None, "foreign_keys", true).unwrap();
        assert_eq!(smallclaims::store::checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}
