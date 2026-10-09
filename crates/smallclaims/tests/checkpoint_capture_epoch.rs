//! Checkpoint page fences persist and follow committed mutations on every connection.
use rusqlite::Connection;
use smallclaims::{
    Store,
    claim::ClaimInput,
    store::{checkpoint_capture_epoch, configure_projection_writer, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

fn append(store: &Store, label: &str) -> smallclaims::claim::ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: format!("note/{label}"),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

fn epoch(store: &Store) -> i64 {
    checkpoint_capture_epoch(&store.readers.get()).unwrap()
}

fn capture_prefix(store: &Store) -> (i64, i64) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let high = smallclaims::store::max_envelope_rowid(&tx).unwrap();
    tx.execute(
        "UPDATE checkpoint_capture_epoch SET envelope_frontier=MAX(envelope_frontier,?1) WHERE id=1",
        [high],
    )
    .unwrap();
    let value = checkpoint_capture_epoch(&tx).unwrap();
    tx.commit().unwrap();
    (high, value)
}

#[test]
fn ordinary_append_does_not_invalidate_the_captured_prefix() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (high, before) = capture_prefix(&store);
    assert!(high > 0);
    let later = append(&store, "later");
    assert_eq!(epoch(&store), before, "ordinary claim append invalidated capture");
    store.seal_local_batches().unwrap();
    assert!(smallclaims::store::max_envelope_rowid(&store.readers.get()).unwrap() > high);
    assert_eq!(epoch(&store), before, "new envelope/records invalidated capture");
    assert!(store.claim_by_id(&later.id).unwrap().is_some());
}

#[test]
fn pending_envelope_record_backfill_invalidates_the_captured_prefix() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    store.connection.write().execute_batch(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'pending','0',x'','remote','pending','0');",
    ).unwrap();
    let (_, before) = capture_prefix(&store);
    store.connection.write().execute_batch(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,updated_at_unix_ms)
         VALUES('record/pending','remote',1,'pending',0,x'','unknown','0');",
    ).unwrap();
    assert!(epoch(&store) > before, "delayed admission changed a sealed envelope without a fence");
}

#[test]
fn a_new_envelope_record_naming_a_captured_claim_invalidates_canonical_metadata() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let claim = append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let writer = store.connection.write();
    writer.execute_batch(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'duplicate','0',x'','remote','pending','0');",
    ).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    writer.execute(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,claim_id,updated_at_unix_ms)
         VALUES('record/duplicate','remote',1,'duplicate',0,x'','valid',?1,'0')",
        [&claim.id],
    ).unwrap();
    assert!(checkpoint_capture_epoch(&writer).unwrap() > before);
}

#[test]
fn late_claim_body_for_a_captured_record_invalidates_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let writer = store.connection.write();
    writer.execute_batch(
        "INSERT INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         VALUES('backfill','remote',1,'batch','0');
         INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'backfill','0',x'','remote','pending','0');
         INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,claim_id,updated_at_unix_ms)
         VALUES('record/backfill','remote',1,'backfill',0,x'','unknown','late-claim','0');",
    ).unwrap();
    drop(writer);
    let (_, before) = capture_prefix(&store);
    store.connection.write().execute_batch(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,body,predecessors,accepted_at_unix_ms)
         VALUES('late-claim','backfill','note/backfill','example.note','remote','{}','[]','0');",
    ).unwrap();
    assert!(epoch(&store) > before);
}

#[test]
fn repair_claim_append_invalidates_protection_without_waiting_for_record_repair() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let replacement = append(&store, "replacement");
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    store.append_claim(&ClaimInput {
        subject: "repair/protection".into(),
        kind: "record.repaired".into(),
        actor: None,
        fields: BTreeMap::from([("replacement".into(), serde_json::json!(replacement.id))]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    assert!(epoch(&store) > before);
}

#[test]
fn every_captured_core_table_update_and_delete_invalidates_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    capture_prefix(&store);
    // Rollback keeps each fixture available and proves the epoch is part of that transaction.
    for (table, column) in [
        ("claims", "body"),
        ("batches", "origin"),
        ("replica_records", "state"),
        ("replica_envelopes", "receipt_state"),
    ] {
        let mut writer = store.connection.write();
        let before = checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(&format!("UPDATE {table} SET {column}={column}"), []).unwrap();
        assert!(checkpoint_capture_epoch(&tx).unwrap() > before, "{table} update");
        tx.rollback().unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
        // Disable FK enforcement only for this isolated trigger-coverage deletion.
        writer.pragma_update(None, "foreign_keys", false).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(&format!("DELETE FROM {table}"), []).unwrap();
        assert!(checkpoint_capture_epoch(&tx).unwrap() > before, "{table} delete");
        tx.rollback().unwrap();
        writer.pragma_update(None, "foreign_keys", true).unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}

#[test]
fn independent_connection_commit_invalidates_guard_but_rollback_does_not() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("claims.sqlite3");
    let store = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    let claim = append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (high, before) = capture_prefix(&store);
    let mut external = Connection::open(&path).unwrap();
    configure_projection_writer(&external).unwrap();
    let tx = external.transaction().unwrap();
    tx.execute("UPDATE claims SET actor='person/other' WHERE id=?1", [&claim.id]).unwrap();
    assert!(checkpoint_capture_epoch(&tx).unwrap() > before);
    assert_eq!(epoch(&store), before, "uncommitted writer leaked an epoch");
    tx.rollback().unwrap();
    assert_eq!(epoch(&store), before);
    let tx = external.transaction().unwrap();
    tx.execute("UPDATE claims SET actor='person/other' WHERE id=?1", [&claim.id]).unwrap();
    tx.commit().unwrap();
    let committed = epoch(&store);
    assert!(committed > before, "fresh page guard accepted a stale capture");
    drop(store);
    let reopened = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    assert_eq!(epoch(&reopened), committed, "reopen reset the mutation epoch");
    assert_eq!(
        reopened.readers.get().query_row(
            "SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1", [], |row| row.get::<_, i64>(0),
        ).unwrap(),
        high,
    );
}

#[test]
fn insertion_at_an_existing_capture_frontier_invalidates_even_without_records() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let writer = store.connection.write();
    writer.execute("UPDATE checkpoint_capture_epoch SET envelope_frontier=10 WHERE id=1", []).unwrap();
    let before = checkpoint_capture_epoch(&writer).unwrap();
    writer.execute(
        "INSERT INTO replica_envelopes(rowid,writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES(10,'remote',1,'late','0',x'','remote','pending','0')", [],
    ).unwrap();
    assert!(checkpoint_capture_epoch(&writer).unwrap() > before);
}

#[test]
fn schema_upgrade_initializes_epoch_and_triggers_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("claims.sqlite3");
    let legacy = Connection::open(&path).unwrap();
    legacy.execute_batch(smallclaims::store::SCHEMA).unwrap();
    legacy.execute_batch("DROP TABLE checkpoint_capture_epoch; PRAGMA user_version=17;").unwrap();
    drop(legacy);
    let store = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    assert_eq!(epoch(&store), 0);
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (high, before) = capture_prefix(&store);
    store.connection.write().execute("UPDATE replica_envelopes SET receipt_state='degraded'", []).unwrap();
    let changed = epoch(&store);
    assert!(changed > before);
    let triggers: Vec<String> = store.readers.get().prepare(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE '%_checkpoint_capture_%' ORDER BY name",
    ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    drop(store);
    let reopened = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    assert_eq!(epoch(&reopened), changed);
    assert_eq!(reopened.readers.get().query_row(
        "SELECT envelope_frontier FROM checkpoint_capture_epoch WHERE id=1", [], |row| row.get::<_, i64>(0),
    ).unwrap(), high);
    let after: Vec<String> = reopened.readers.get().prepare(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE '%_checkpoint_capture_%' ORDER BY name",
    ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(after, triggers);
    assert_eq!(reopened.readers.get().query_row(
        "SELECT COUNT(*) FROM checkpoint_capture_epoch", [], |row| row.get::<_, u64>(0),
    ).unwrap(), 1);
}

#[test]
fn all_tombstone_and_document_protection_mutations_invalidate_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let writer = store.connection.write();
    writer.execute("INSERT INTO blobs(hash,bytes,size) VALUES('blob',x'',0)", []).unwrap();
    for (table, insert, column) in [
        ("checkpoint_claims", "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,predecessors,accepted_at_unix_ms,checkpoint) VALUES('claim','remote',1,'hash','note/test','example.note','[]',0,'checkpoint/test')", "kind"),
        ("checkpoint_envelopes", "INSERT INTO checkpoint_envelopes VALUES('remote',1,'hash',0,'checkpoint/test')", "checkpoint"),
        ("documents", "INSERT INTO documents(name,hash,created_index,binding_claim_id) VALUES('document','blob',1,'claim')", "binding_claim_id"),
    ] {
        let before = checkpoint_capture_epoch(&writer).unwrap();
        writer.execute(insert, []).unwrap();
        let inserted = checkpoint_capture_epoch(&writer).unwrap();
        assert!(inserted > before, "{table} insert");
        writer.execute(&format!("UPDATE {table} SET {column}={column}"), []).unwrap();
        let updated = checkpoint_capture_epoch(&writer).unwrap();
        assert!(updated > inserted, "{table} update");
        writer.execute(&format!("DELETE FROM {table}"), []).unwrap();
        assert!(checkpoint_capture_epoch(&writer).unwrap() > updated, "{table} delete");
    }
}

#[test]
fn replace_cannot_bypass_the_fence_with_recursive_triggers_disabled() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let first = append(&store, "first");
    append(&store, "second");
    store.seal_local_batches().unwrap();
    capture_prefix(&store);
    let mut writer = store.connection.write();
    writer.pragma_update(None, "recursive_triggers", false).unwrap();
    writer.pragma_update(None, "foreign_keys", false).unwrap();
    for sql in [
        "INSERT OR REPLACE INTO replica_envelopes(rowid,writer,sequence,envelope_hash,
             previous_hash,accepted_at_unix_ms,payload,batch_id,relay,receipt_state,received_at_unix_ms)
         SELECT 100,writer,sequence,envelope_hash,previous_hash,accepted_at_unix_ms,
             x'',batch_id,relay,receipt_state,received_at_unix_ms
         FROM replica_envelopes WHERE rowid=(SELECT MIN(rowid) FROM replica_envelopes)",
        "INSERT OR REPLACE INTO replica_records(record_ref,writer,sequence,envelope_hash,
             position,raw,state,claim_id,updated_at_unix_ms)
         SELECT record_ref,'later',100,'later',position,x'','valid',NULL,'0'
         FROM replica_records WHERE claim_id=?1",
        "INSERT OR REPLACE INTO claims(store_index,id,batch_id,subject,kind,origin,body,
             predecessors,accepted_at_unix_ms)
         SELECT store_index,'changed-identity',batch_id,subject,kind,origin,'{}','[]',
             accepted_at_unix_ms FROM claims WHERE id=?1",
        "INSERT OR REPLACE INTO batches(id,origin,replica_sequence,hash,accepted_at_unix_ms)
         SELECT id,'changed-writer',replica_sequence,hash,accepted_at_unix_ms FROM batches",
    ] {
        let before = checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        if sql.contains("?1") {
            tx.execute(sql, [&first.id]).unwrap();
        } else {
            tx.execute(sql, []).unwrap();
        }
        assert!(checkpoint_capture_epoch(&tx).unwrap() > before, "{sql}");
        tx.rollback().unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}

#[test]
fn applying_replica_repair_advances_the_epoch_in_the_record_mutation_transaction() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let original = append(&store, "original");
    let replacement = append(&store, "replacement");
    store.seal_local_batches().unwrap();
    let record: String = store.readers.get().query_row(
        "SELECT record_ref FROM replica_records WHERE claim_id=?1", [&original.id],
        |row| row.get(0),
    ).unwrap();
    store.append_claim(&ClaimInput {
        subject: "repair/capture".into(),
        kind: "record.repaired".into(),
        actor: None,
        fields: BTreeMap::from([
            ("record".into(), serde_json::json!(record)),
            ("replacement".into(), serde_json::json!(replacement.id)),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    let (_, before) = capture_prefix(&store);
    assert_eq!(store.apply_replication_repairs().unwrap(), 1);
    assert!(epoch(&store) > before, "record repair committed without invalidating capture");
    assert_eq!(store.readers.get().query_row(
        "SELECT state,replacement_claim_id FROM replica_records WHERE record_ref=?1",
        [&record], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).unwrap(), ("repaired".into(), replacement.id));
}

#[test]
fn later_repaired_and_replacement_records_invalidate_global_protection() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let writer = store.connection.write();
    writer.execute_batch(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'later','0',x'','remote','pending','0');",
    ).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    writer.execute_batch(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,updated_at_unix_ms)
         VALUES('record/repaired','remote',1,'later',0,x'','repaired','0');",
    ).unwrap();
    let repaired = checkpoint_capture_epoch(&writer).unwrap();
    assert!(repaired > before);
    writer.execute_batch(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,replacement_claim_id,updated_at_unix_ms)
         VALUES('record/replacement','remote',1,'later',1,x'','valid','protected-claim','0');",
    ).unwrap();
    assert!(checkpoint_capture_epoch(&writer).unwrap() > repaired);
}
