use smallclaims::store::{Store, full_compact_replication_inventory, runtime::Plain};
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
    assert_eq!(snapshot.buckets.as_ref(), &full.buckets());
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
fn graph_only_rebuild_shares_inventory_with_retained_reader() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    insert(&store, 1, 1);
    insert(&store, 2, 2);
    let retained = check(&store);

    // Model a committed projection change without changing the envelope inventory.
    store
        .connection
        .write()
        .execute("UPDATE graph_generation SET value=value+1 WHERE id=1", [])
        .unwrap();
    let rebuilt = check(&store);
    assert!(!Arc::ptr_eq(&retained, &rebuilt));
    assert!(Arc::ptr_eq(&retained.inventory, &rebuilt.inventory));
    assert!(Arc::ptr_eq(&retained.buckets, &rebuilt.buckets));
    assert!(Arc::ptr_eq(
        &retained.digest_prefixes,
        &rebuilt.digest_prefixes
    ));

    insert(&store, 3, 3);
    let appended = check(&store);
    assert_eq!(retained.inventory.envelopes.len(), 2);
    assert_eq!(appended.inventory.envelopes.len(), 3);
    assert_ne!(retained.inventory.digest, appended.inventory.digest);
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
