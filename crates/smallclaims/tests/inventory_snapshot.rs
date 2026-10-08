use smallclaims::store::{
    Store, full_compact_replication_inventory, graph_digest, legacy_graph_digest, runtime::Plain,
};
use smallclaims::claim::ClaimInput;
use std::collections::BTreeMap;
use std::sync::Arc;

fn insert(store: &Store, rowid: i64, sequence: i64) {
    store
        .connection
        .write()
        .execute(
            "INSERT INTO replica_envelopes(rowid,writer,sequence,envelope_hash,accepted_at_unix_ms,
         payload,relay,receipt_state,received_at_unix_ms)
         VALUES(?1,'alder',?2,?3,'0',X'','alder','pending','0')",
            rusqlite::params![rowid, sequence, format!("hash-{sequence}")],
        )
        .unwrap();
}

fn check(store: &Store) -> Arc<smallclaims::store::ReplicationSnapshot> {
    let snapshot = store.sealed_replication_snapshot().unwrap();
    let connection = store.connection.write();
    let (full, max_rowid) = full_compact_replication_inventory(&connection).unwrap();
    let held: usize = connection
        .query_row("SELECT COUNT(*) FROM replica_envelopes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snapshot.envelope_rows, held);
    assert_eq!(snapshot.max_envelope_rowid, max_rowid);
    assert_eq!(snapshot.inventory.digest, full.digest);
    assert_eq!(snapshot.buckets, full.buckets());
    assert_eq!(snapshot.graph_digest, graph_digest(&connection).unwrap());
    assert_eq!(snapshot.legacy_graph_digest,
        legacy_graph_digest(&connection, store.runtime.legacy_digest_tables()).unwrap());
    for envelope in &snapshot.inventory.envelopes {
        let identity = snapshot.inventory.identity(envelope);
        let corresponding = full
            .envelopes
            .iter()
            .find(|e| full.identity(e) == identity)
            .unwrap();
        assert_eq!(
            snapshot.inventory.has_payload(envelope),
            full.has_payload(corresponding)
        );
    }
    snapshot
}

#[test]
fn incremental_snapshot_tracks_prefix_edits_checkpoint_availability_and_rollback() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    insert(&store, 1, 1);
    insert(&store, 3, 3);
    let original = check(&store);
    insert(&store, 4, 4);
    let appended = check(&store);
    assert_eq!(appended.inventory_generation, original.inventory_generation);
    assert_eq!(original.envelope_rows, 2);
    assert_eq!(original.inventory.envelopes.len(), 2);

    // Filling a hole and deleting a prefix both leave the existing rowid tail unchanged.
    insert(&store, 2, 2);
    let filled = check(&store);
    assert_ne!(filled.inventory_generation, appended.inventory_generation);
    store
        .connection
        .write()
        .execute("DELETE FROM replica_envelopes WHERE rowid=1", [])
        .unwrap();
    check(&store);
    store
        .connection
        .write()
        .execute(
            "UPDATE replica_envelopes SET envelope_hash='changed' WHERE rowid=2",
            [],
        )
        .unwrap();
    let edited = check(&store);

    store
        .connection
        .write()
        .execute_batch(
            "SAVEPOINT sample; DELETE FROM replica_envelopes WHERE rowid=2;
         ROLLBACK TO sample; RELEASE sample;",
        )
        .unwrap();
    assert!(Arc::ptr_eq(&edited, &check(&store)));

    store
        .connection
        .write()
        .execute_batch(
            "INSERT INTO checkpoint_envelopes VALUES('birch',8,'dropped',0,'checkpoint/sample');",
        )
        .unwrap();
    check(&store);
    // Updating an identity preserves the tombstone count but changes the inventory.
    store
        .connection
        .write()
        .execute(
            "UPDATE checkpoint_envelopes SET envelope_hash='different'",
            [],
        )
        .unwrap();
    check(&store);
    store
        .connection
        .write()
        .execute("DELETE FROM checkpoint_envelopes", [])
        .unwrap();
    check(&store);
}

#[test]
fn reopening_preserves_mutation_tracking_and_existing_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sample.sqlite3");
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    insert(&store, 1, 1);
    insert(&store, 2, 2);
    let indexes = |store: &Store| -> Vec<(String, Option<String>)> {
        store
            .connection
            .write()
            .prepare("SELECT name,sql FROM sqlite_master WHERE type='index' ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = indexes(&store);
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    check(&store);
    store
        .connection
        .write()
        .execute("DELETE FROM replica_envelopes WHERE rowid=1", [])
        .unwrap();
    check(&store);
    assert_eq!(indexes(&store), before);
}

#[test]
fn older_pinned_reader_does_not_reuse_or_replace_a_newer_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&directory.path().join("pinned.sqlite3"), "alder", Arc::new(Plain)).unwrap(),
    );
    insert(&store, 1, 1);
    let first = check(&store);
    store.read_snapshot(|_| {
        let writer = Arc::clone(&store);
        let newest = std::thread::spawn(move || {
            insert(&writer, 2, 2);
            writer.sealed_replication_snapshot().unwrap()
        }).join().unwrap();
        assert_eq!(newest.envelope_rows, 2);
        assert_ne!(newest.inventory.digest, first.inventory.digest);

        // The older reader must build from its own SQLite cut even after the shared cache
        // contains the newer suffix, and must not publish its old result over that cache.
        let pinned = store.sealed_replication_snapshot()?;
        assert_eq!(pinned.envelope_rows, 1);
        assert_eq!(pinned.inventory.digest, first.inventory.digest);
        assert_eq!(pinned.authority_digest, first.authority_digest);
        Ok(())
    }).unwrap();
    let current = check(&store);
    assert_eq!(current.envelope_rows, 2);
    assert_ne!(current.inventory.digest, first.inventory.digest);
}

#[test]
fn pending_batch_guard_uses_the_pinned_committed_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&directory.path().join("pending.sqlite3"), "alder", Arc::new(Plain)).unwrap(),
    );
    store.append_claim(&ClaimInput {
        subject: "note/pending".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::new(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    store.read_snapshot(|_| {
        let writer = Arc::clone(&store);
        let fresh = std::thread::spawn(move || {
            writer.seal_local_batches().unwrap();
            writer.sealed_replication_snapshot().unwrap()
        }).join().unwrap();
        assert!(!fresh.unsealed);
        let old = store.sealed_replication_snapshot()?;
        assert!(old.unsealed, "a newer live sealing cursor hid the pending pinned batch");
        assert_ne!(old.inventory.digest, fresh.inventory.digest);
        Ok(())
    }).unwrap();
}

#[test]
fn reopened_trimmed_tail_persists_the_cursor_clamp_before_rowid_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trimmed-tail.sqlite3");
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    {
        let connection = store.connection.write();
        connection.execute(
            "INSERT INTO batches(rowid,id,origin,replica_sequence,hash,accepted_at_unix_ms)
             VALUES(100,'trimmed','other',100,'trimmed-hash','0')",
            [],
        ).unwrap();
        connection.execute("DELETE FROM batches WHERE id='trimmed'", []).unwrap();
        connection.execute(
            "UPDATE meta SET value='100' WHERE key='seeded_batch_rowid'", [],
        ).unwrap();
    }
    drop(store);

    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let persisted: i64 = store.connection.write().query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key='seeded_batch_rowid'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(persisted, 0, "the startup clamp must be visible to pinned readers");
    store.append_claim(&ClaimInput {
        subject: "note/after-trim".into(),
        kind: "example.note".into(),
        actor: None,
        fields: BTreeMap::new(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    }).unwrap();
    let rowid: i64 = store.connection.write().query_row(
        "SELECT MAX(rowid) FROM batches", [], |row| row.get(0),
    ).unwrap();
    assert!(rowid > persisted && rowid < 100, "the new batch reused the trimmed rowid range");
    assert!(store.sealed_replication_snapshot().unwrap().unsealed);
}
