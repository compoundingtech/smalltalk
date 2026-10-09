//! Checkpoint page fences persist and follow committed mutations on every connection.
use rusqlite::Connection;
use smallclaims::{
    Store,
    claim::ClaimInput,
    store::{checkpoint_capture_epoch, configure_projection_writer, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

const CUT: i64 = 150;

fn append_at(store: &Store, label: &str, accepted: i64) -> smallclaims::claim::ClaimRecord {
    let claim = store
        .append_claim(&ClaimInput {
            subject: format!("note/{label}"),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let writer = store.connection.write();
    writer.execute(
        "UPDATE claims SET accepted_at_unix_ms=?2 WHERE id=?1",
        rusqlite::params![claim.id, accepted.to_string()],
    ).unwrap();
    writer.execute(
        "UPDATE batches SET accepted_at_unix_ms=?2 WHERE id=?1",
        rusqlite::params![claim.batch_id, accepted.to_string()],
    ).unwrap();
    claim
}

fn append(store: &Store, label: &str) -> smallclaims::claim::ClaimRecord {
    append_at(store, label, 100)
}

fn epoch(store: &Store) -> i64 {
    checkpoint_capture_epoch(&store.readers.get()).unwrap()
}

fn capture_prefix(store: &Store) -> (i64, i64) {
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    let high = smallclaims::store::max_envelope_rowid(&tx).unwrap();
    tx.execute(
        "UPDATE checkpoint_capture_epoch
         SET envelope_frontier=MAX(envelope_frontier,?1),cut_unix_ms=MAX(cut_unix_ms,?2)
         WHERE id=1 AND (envelope_frontier<?1 OR cut_unix_ms<?2)",
        [high, CUT],
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
    let later = append_at(&store, "later", 200);
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
         VALUES('remote',1,'duplicate','200',x'','remote','pending','0');",
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
        ("claims", "body=body||' '"),
        ("batches", "origin=origin||'/changed'"),
        ("replica_records", "state='unknown'"),
        ("replica_envelopes", "accepted_at_unix_ms='101'"),
    ] {
        let mut writer = store.connection.write();
        let before = checkpoint_capture_epoch(&writer).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(&format!("UPDATE {table} SET {column}"), []).unwrap();
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
    writer.execute(
        "UPDATE checkpoint_capture_epoch SET envelope_frontier=10,cut_unix_ms=?1 WHERE id=1",
        [CUT],
    ).unwrap();
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
    assert_eq!(epoch(&store), 1, "installing the guard version advances the epoch");
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (high, before) = capture_prefix(&store);
    store.connection.write().execute("UPDATE replica_envelopes SET accepted_at_unix_ms='101'", []).unwrap();
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
fn all_below_cut_tombstone_and_document_protection_mutations_invalidate_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let claim = append(&store, "captured");
    let other = append(&store, "other");
    store.seal_local_batches().unwrap();
    capture_prefix(&store);
    let writer = store.connection.write();
    writer.execute("INSERT INTO blobs(hash,bytes,size) VALUES('blob',x'',0)", []).unwrap();
    for (table, insert, change) in [
        ("checkpoint_claims", "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,predecessors,accepted_at_unix_ms,checkpoint) VALUES('tombstone','remote',1,'hash','note/test','example.note','[]',0,'checkpoint/test')".to_owned(), "kind=kind||'/changed'".to_owned()),
        ("checkpoint_envelopes", "INSERT INTO checkpoint_envelopes VALUES('remote',1,'hash',0,'checkpoint/test')".to_owned(), "accepted_at_unix_ms=1".to_owned()),
        ("documents", format!("INSERT INTO documents(name,hash,created_index,binding_claim_id) VALUES('document','blob',1,'{}')", claim.id), format!("binding_claim_id='{}'", other.id)),
    ] {
        let before = checkpoint_capture_epoch(&writer).unwrap();
        writer.execute(&insert, []).unwrap();
        let inserted = checkpoint_capture_epoch(&writer).unwrap();
        assert!(inserted > before, "{table} insert");
        writer.execute(&format!("UPDATE {table} SET {change}"), []).unwrap();
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
fn later_repair_references_only_invalidate_captured_below_cut_targets() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let captured = append(&store, "captured");
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let writer = store.connection.write();
    writer.execute_batch(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'later','200',x'','remote','pending','0');
         INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,updated_at_unix_ms)
         VALUES('record/repaired','remote',1,'later',0,x'','repaired','0');",
    ).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    writer.execute(
        "UPDATE replica_records SET replacement_claim_id=?1 WHERE record_ref='record/repaired'",
        [&newer.id],
    ).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    writer.execute(
        "UPDATE replica_records SET replacement_claim_id=?1 WHERE record_ref='record/repaired'",
        [&captured.id],
    ).unwrap();
    let protected = checkpoint_capture_epoch(&writer).unwrap();
    assert!(protected > before);
    writer.execute(
        "UPDATE replica_records SET replacement_claim_id=?1 WHERE record_ref='record/repaired'",
        [&newer.id],
    ).unwrap();
    assert!(checkpoint_capture_epoch(&writer).unwrap() > protected, "OLD captured protection was lost");
}

#[test]
fn replicated_envelope_admission_updates_above_cut_do_not_invalidate_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    // A newer envelope can already lie inside the registered row frontier.
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let writer = store.connection.write();
    writer.execute(
        "UPDATE replica_envelopes SET receipt_state='pending',batch_id=NULL WHERE batch_id=?1",
        [&newer.batch_id],
    ).unwrap();
    writer.execute(
        "UPDATE replica_envelopes SET receipt_state='validated',batch_id=?1,
             relay='another-relay',received_at_unix_ms='201'
         WHERE accepted_at_unix_ms='200'",
        [&newer.batch_id],
    ).unwrap();
    writer.execute(
        "UPDATE replica_records SET state='unknown',updated_at_unix_ms='201' WHERE claim_id=?1",
        [&newer.id],
    ).unwrap();
    writer.execute("UPDATE claims SET body=body||' ' WHERE id=?1", [&newer.id]).unwrap();
    writer.execute("UPDATE batches SET origin='remote' WHERE id=?1", [&newer.batch_id]).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    // Receipt bookkeeping is not captured even for an older envelope.
    writer.execute(
        "UPDATE replica_envelopes SET receipt_state='degraded',validation_error='diagnostic'
         WHERE accepted_at_unix_ms='100'",
        [],
    ).unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
}

#[test]
fn identical_duplicate_ignore_and_no_op_updates_do_not_invalidate_capture() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let writer = store.connection.write();
    for recursive in [false, true] {
        writer.pragma_update(None, "recursive_triggers", recursive).unwrap();
        for sql in [
            "INSERT OR IGNORE INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
             SELECT id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms FROM claims",
            "INSERT OR IGNORE INTO batches SELECT * FROM batches",
            "INSERT OR IGNORE INTO replica_records SELECT * FROM replica_records",
            "INSERT OR IGNORE INTO replica_envelopes SELECT * FROM replica_envelopes",
            "UPDATE claims SET body=body",
            "UPDATE batches SET origin=origin",
            "UPDATE replica_records SET state=state",
            "UPDATE replica_envelopes SET accepted_at_unix_ms=accepted_at_unix_ms",
        ] {
            writer.execute(sql, []).unwrap();
            assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before, "{sql}");
        }
    }
}

#[test]
fn identical_below_cut_envelope_replace_cannot_move_outside_the_frontier_unfenced() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (frontier, before) = capture_prefix(&store);
    let writer = store.connection.write();
    writer.pragma_update(None, "recursive_triggers", false).unwrap();
    writer.execute("INSERT OR REPLACE INTO replica_envelopes SELECT * FROM replica_envelopes", []).unwrap();
    assert!(
        smallclaims::store::max_envelope_rowid(&writer).unwrap() > frontier,
        "REPLACE fixture did not relocate the envelope",
    );
    assert!(checkpoint_capture_epoch(&writer).unwrap() > before);
}

#[test]
fn above_cut_duplicate_record_position_changes_and_deletions_fence_canonical_minimum() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let first = append(&store, "first");
    let captured = append(&store, "captured");
    store.seal_local_batches().unwrap();
    store.connection.write().execute(
        "UPDATE replica_records SET position=2 WHERE claim_id=?1", [&captured.id],
    ).unwrap();
    let (_, before) = capture_prefix(&store);
    let mut writer = store.connection.write();
    writer.execute_batch(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,
             payload,relay,receipt_state,received_at_unix_ms)
         VALUES('remote',1,'duplicate','200',x'','remote','pending','200');",
    ).unwrap();
    writer.execute(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,
             raw,state,claim_id,updated_at_unix_ms)
         VALUES('record/duplicate','remote',1,'duplicate',0,x'','valid',?1,'200')",
        [&captured.id],
    ).unwrap();
    let inserted = checkpoint_capture_epoch(&writer).unwrap();
    assert!(inserted > before);
    let position: i64 = writer.query_row(
        "SELECT MIN(position) FROM replica_records WHERE claim_id=?1", [&captured.id], |row| row.get(0),
    ).unwrap();
    assert_eq!(position, 0);
    for sql in [
        "UPDATE replica_records SET position=2 WHERE record_ref='record/duplicate'",
        "DELETE FROM replica_records WHERE record_ref='record/duplicate'",
        "UPDATE replica_records SET claim_id=?1 WHERE record_ref='record/duplicate'",
    ] {
        let tx = writer.transaction().unwrap();
        if sql.contains("?1") {
            tx.execute(sql, [&first.id]).unwrap();
        } else {
            tx.execute(sql, []).unwrap();
        }
        assert!(checkpoint_capture_epoch(&tx).unwrap() > inserted, "{sql}");
        let minimum: i64 = tx.query_row(
            "SELECT MIN(position) FROM replica_records WHERE claim_id=?1", [&captured.id], |row| row.get(0),
        ).unwrap();
        assert_eq!(minimum, 2, "newer duplicate changed the captured canonical key");
        tx.rollback().unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), inserted);
    }
}

#[test]
fn late_claim_identity_and_time_changes_fence_envelope_exclusion() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let late = append_at(&store, "late", 200);
    store.seal_local_batches().unwrap();
    store.connection.write().execute(
        "UPDATE replica_envelopes SET accepted_at_unix_ms='100'", [],
    ).unwrap();
    let (_, before) = capture_prefix(&store);
    let mut writer = store.connection.write();
    writer.pragma_update(None, "foreign_keys", false).unwrap();
    for sql in [
        "UPDATE claims SET accepted_at_unix_ms='100' WHERE id=?1",
        "UPDATE claims SET batch_id='different-batch' WHERE id=?1",
        "DELETE FROM claims WHERE id=?1",
    ] {
        let tx = writer.transaction().unwrap();
        tx.execute(sql, [&late.id]).unwrap();
        assert!(checkpoint_capture_epoch(&tx).unwrap() > before, "{sql}");
        tx.rollback().unwrap();
    }
    let tx = writer.transaction().unwrap();
    let error = tx.execute(
        "UPDATE claims SET id='different-id' WHERE id=?1", [&late.id],
    ).unwrap_err();
    assert_eq!(
        error.sqlite_error().unwrap().extended_code,
        rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
    );
    assert!(error.to_string().contains("claim source identity is immutable"));
    assert_eq!(
        checkpoint_capture_epoch(&tx).unwrap(), before,
        "rejected identity mutation must not invalidate capture",
    );
    tx.rollback().unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
}

#[test]
fn above_cut_tombstone_mutations_and_unrelated_repair_receipts_do_not_invalidate() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    store.connection.write().execute_batch(
        "INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
             predecessors,accepted_at_unix_ms,checkpoint)
         VALUES('newer-tombstone','remote',1,'hash','note/test','example.note','[]',200,'checkpoint/test');
         INSERT INTO checkpoint_envelopes VALUES('remote',1,'hash',200,'checkpoint/test');
         UPDATE checkpoint_claims SET kind='changed';
         UPDATE checkpoint_envelopes SET accepted_at_unix_ms=201;
         DELETE FROM checkpoint_claims;
         DELETE FROM checkpoint_envelopes;",
    ).unwrap();
    store.append_claim(&ClaimInput {
        subject: "repair/unrelated".into(),
        kind: "record.repaired".into(),
        actor: None,
        fields: BTreeMap::from([("replacement".into(), serde_json::json!(newer.id))]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    assert_eq!(epoch(&store), before);
    store.connection.write().execute_batch(
        "INSERT INTO checkpoint_envelopes VALUES('remote',1,'hash',200,'checkpoint/test');",
    ).unwrap();
    store.connection.write().execute(
        "UPDATE checkpoint_envelopes SET accepted_at_unix_ms=100", [],
    ).unwrap();
    assert!(epoch(&store) > before, "NEW below-cut tombstone was not fenced");
}

#[test]
fn repair_record_reference_and_old_replacement_reference_invalidate_below_cut_targets() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let captured = append(&store, "captured");
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    let record: String = store.readers.get().query_row(
        "SELECT record_ref FROM replica_records WHERE claim_id=?1", [&captured.id], |row| row.get(0),
    ).unwrap();
    let (_, before) = capture_prefix(&store);
    let receipt = store.append_claim(&ClaimInput {
        subject: "repair/captured".into(),
        kind: "record.repaired".into(),
        actor: None,
        fields: BTreeMap::from([
            ("record".into(), serde_json::json!(record)),
            ("replacement".into(), serde_json::json!(newer.id)),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    assert!(epoch(&store) > before, "repair referenced a captured record");
    let writer = store.connection.write();
    writer.execute(
        "UPDATE claims SET body=json_set(body,'$.fields.record','unrelated',
             '$.fields.replacement',?2) WHERE id=?1",
        rusqlite::params![receipt.id, captured.id],
    ).unwrap();
    let protected = checkpoint_capture_epoch(&writer).unwrap();
    writer.execute(
        "UPDATE claims SET body=json_set(body,'$.fields.replacement',?2) WHERE id=?1",
        rusqlite::params![receipt.id, newer.id],
    ).unwrap();
    assert!(checkpoint_capture_epoch(&writer).unwrap() > protected, "OLD repair protection disappeared");
}

#[test]
fn old_version18_trigger_upgrade_preserves_frontier_and_atomically_advances_epoch() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("claims.sqlite3");
    let store = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    store.seal_local_batches().unwrap();
    let (frontier, _) = capture_prefix(&store);
    drop(store);
    let legacy = Connection::open(&path).unwrap();
    let triggers: Vec<String> = legacy.prepare(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE '%_checkpoint_capture_%'",
    ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    for name in triggers {
        legacy.execute_batch(&format!("DROP TRIGGER {name};")).unwrap();
    }
    legacy.execute_batch(
        "DROP TABLE checkpoint_capture_epoch;
         CREATE TABLE checkpoint_capture_epoch(
             id INTEGER PRIMARY KEY CHECK(id=1),value INTEGER NOT NULL,
             envelope_frontier INTEGER NOT NULL DEFAULT 0);
         CREATE TRIGGER claims_checkpoint_capture_UPDATE AFTER UPDATE ON claims BEGIN
             UPDATE checkpoint_capture_epoch SET value=value+1 WHERE id=1;
         END;
         PRAGMA user_version=18;",
    ).unwrap();
    legacy.execute(
        "INSERT INTO checkpoint_capture_epoch VALUES(1,41,?1)", [frontier],
    ).unwrap();
    drop(legacy);
    let upgraded = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    let bounds: (i64, i64, i64, i64) = upgraded.readers.get().query_row(
        "SELECT value,envelope_frontier,cut_unix_ms,trigger_version FROM checkpoint_capture_epoch WHERE id=1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(bounds, (42, frontier, 0, 2));
    upgraded.connection.write().execute("UPDATE claims SET body=body||' '", []).unwrap();
    assert_eq!(epoch(&upgraded), 42, "legacy default cut zero must remain inactive");
    capture_prefix(&upgraded);
    let before = epoch(&upgraded);
    upgraded.connection.write().execute("UPDATE claims SET body=body", []).unwrap();
    assert_eq!(epoch(&upgraded), before, "blanket guard-v1 trigger survived migration");
    drop(upgraded);
    let reopened = Store::open(&path, "writer", Arc::new(Plain)).unwrap();
    let bounds: (i64, i64, i64, i64) = reopened.readers.get().query_row(
        "SELECT value,envelope_frontier,cut_unix_ms,trigger_version FROM checkpoint_capture_epoch WHERE id=1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).unwrap();
    assert_eq!(bounds, (before, frontier, CUT, 2), "reopen rewrote the guard");
}

#[test]
fn tombstone_and_document_replace_fence_old_below_cut_content_not_identical_ignores() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    let captured = append(&store, "captured");
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    capture_prefix(&store);
    let mut writer = store.connection.write();
    writer.pragma_update(None, "recursive_triggers", false).unwrap();
    writer.execute_batch(
        "INSERT INTO blobs(hash,bytes,size) VALUES('blob',x'',0);
         INSERT INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
             predecessors,accepted_at_unix_ms,checkpoint)
         VALUES('tombstone','remote',1,'hash','note/test','example.note','[]',100,'checkpoint/test');
         INSERT INTO checkpoint_envelopes VALUES('remote',1,'hash',100,'checkpoint/test');",
    ).unwrap();
    writer.execute(
        "INSERT INTO documents(name,hash,created_index,binding_claim_id) VALUES('document','blob',1,?1)",
        [&captured.id],
    ).unwrap();
    let before = checkpoint_capture_epoch(&writer).unwrap();
    for sql in [
        "INSERT OR IGNORE INTO checkpoint_claims SELECT * FROM checkpoint_claims",
        "INSERT OR IGNORE INTO checkpoint_envelopes SELECT * FROM checkpoint_envelopes",
        "INSERT OR IGNORE INTO documents SELECT * FROM documents",
        "UPDATE checkpoint_claims SET kind=kind",
        "UPDATE documents SET binding_claim_id=binding_claim_id",
    ] {
        writer.execute(sql, []).unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before, "{sql}");
    }
    for sql in [
        "INSERT OR REPLACE INTO checkpoint_claims(id,writer,sequence,envelope_hash,subject,kind,
             actor,predecessors,operation_id,request_digest,accepted_at_unix_ms,checkpoint)
         SELECT id,writer,sequence,envelope_hash,subject,kind,actor,predecessors,operation_id,
             request_digest,200,checkpoint FROM checkpoint_claims",
        "INSERT OR REPLACE INTO checkpoint_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,checkpoint)
         SELECT writer,sequence,envelope_hash,200,checkpoint FROM checkpoint_envelopes",
        "INSERT OR REPLACE INTO documents(name,hash,created_index,binding_claim_id,binding_key)
         SELECT name,hash,created_index,?1,binding_key FROM documents",
    ] {
        let tx = writer.transaction().unwrap();
        if sql.contains("?1") {
            tx.execute(sql, [&newer.id]).unwrap();
        } else {
            tx.execute(sql, []).unwrap();
        }
        assert!(checkpoint_capture_epoch(&tx).unwrap() > before, "{sql}");
        tx.rollback().unwrap();
        assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    }
}

#[test]
fn envelope_cut_crossings_fence_both_sides_and_frontier_still_limits_core_rows() {
    let store = Store::open_memory("writer", Arc::new(Plain)).unwrap();
    append(&store, "captured");
    let newer = append_at(&store, "newer", 200);
    store.seal_local_batches().unwrap();
    let (_, before) = capture_prefix(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute(
        "UPDATE replica_envelopes SET accepted_at_unix_ms='100' WHERE batch_id=?1",
        [&newer.batch_id],
    ).unwrap();
    let crossed = checkpoint_capture_epoch(&tx).unwrap();
    assert!(crossed > before, "NEW relevant envelope");
    tx.execute(
        "UPDATE replica_envelopes SET accepted_at_unix_ms='200' WHERE batch_id=?1",
        [&newer.batch_id],
    ).unwrap();
    assert!(checkpoint_capture_epoch(&tx).unwrap() > crossed, "OLD relevant envelope");
    tx.rollback().unwrap();
    assert_eq!(checkpoint_capture_epoch(&writer).unwrap(), before);
    drop(writer);
    let outside = append_at(&store, "outside", 100);
    store.seal_local_batches().unwrap();
    // The below-cut INSERT conservatively invalidates; later core writes above the
    // retained frontier are not part of that capture.
    let after_insert = epoch(&store);
    let mut writer = store.connection.write();
    let tx = writer.transaction().unwrap();
    tx.execute("UPDATE claims SET body=body||' ' WHERE id=?1", [&outside.id]).unwrap();
    tx.execute(
        "UPDATE replica_records SET state='unknown' WHERE claim_id=?1", [&outside.id],
    ).unwrap();
    tx.execute("UPDATE batches SET origin='another-origin' WHERE id=?1", [&outside.batch_id]).unwrap();
    tx.execute("DELETE FROM replica_records WHERE claim_id=?1", [&outside.id]).unwrap();
    assert_eq!(checkpoint_capture_epoch(&tx).unwrap(), after_insert);
    tx.rollback().unwrap();
}
