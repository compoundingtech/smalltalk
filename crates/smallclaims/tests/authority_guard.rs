//! The local guard's reply size and digest semantics, on synthetic Plain stores. These are
//! not CPU profiles, signed-peer qualification or a production idle-hour measurement.
use rusqlite::params;
use smallclaims::{
    claim::ClaimInput,
    replication::ReplicationExchange,
    store::{Store, full_compact_replication_inventory, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

const FLEET: &str = "fixture-fleet";

fn node() -> Store {
    Store::open_memory("alder", Arc::new(Plain)).unwrap()
}

fn identity(store: &Store, writer: &str, sequence: u64) {
    store.connection.write().execute(
        "INSERT INTO replica_envelopes(writer,sequence,envelope_hash,accepted_at_unix_ms,payload,relay,receipt_state,received_at_unix_ms)
         VALUES(?1,?2,?3,'0',X'','fixture','pending','0')",
        params![writer, sequence, format!("hash-{writer}-{sequence:08}")],
    ).unwrap();
}

fn equivalent(store: &Store) -> (ReplicationExchange, ReplicationExchange) {
    // Settle any first-call sealing/judging before the comparison; no writer runs between
    // these requests. A separate fresh table recompute is the inventory digest oracle.
    store.export_replication_summary(FLEET).unwrap();
    let full = store.export_replication_summary(FLEET).unwrap();
    let reduced = store.export_replication_authority_guard(FLEET).unwrap();
    let (fresh, _) = full_compact_replication_inventory(&store.readers.get()).unwrap();
    assert_eq!(full.authority_digest, fresh.digest);
    assert_eq!(reduced.authority_digest, fresh.digest);
    assert_eq!(full.inventory.buckets, fresh.buckets());
    assert!(reduced.inventory.buckets.is_empty());
    let mut expected = serde_json::to_value(&full).unwrap();
    expected["inventory"]
        .as_object_mut()
        .unwrap()
        .remove("buckets");
    assert_eq!(
        serde_json::to_value(&reduced).unwrap(),
        expected,
        "every non-bucket field is unchanged"
    );
    (full, reduced)
}

#[test]
fn empty_mixed_and_one_changed_bucket_match_fresh_inventory_and_full_summary() {
    let store = node();
    equivalent(&store);
    for (writer, sequence) in [("alder", 1), ("alder", 2), ("alder", 257), ("birch", 513)] {
        identity(&store, writer, sequence);
    }
    let (before, _) = equivalent(&store);
    store.connection.write().execute("UPDATE replica_envelopes SET envelope_hash='changed' WHERE writer='alder' AND sequence=257", []).unwrap();
    let (after, _) = equivalent(&store);
    assert_ne!(before.authority_digest, after.authority_digest);
    assert_eq!(
        before
            .inventory
            .buckets
            .iter()
            .zip(&after.inventory.buckets)
            .filter(|(old, new)| old != new)
            .count(),
        1
    );
}

#[test]
fn reduced_reply_bytes_stay_fixed_while_independent_legacy_reply_grows() {
    let mut sizes = Vec::new();
    for buckets in [2, 128] {
        let store = node();
        for index in 0..buckets {
            identity(&store, "alder", 1 + index * 256);
        }
        let (full, reduced) = equivalent(&store);
        let full_bytes = serde_json::to_vec(&full).unwrap().len();
        let reduced_bytes = serde_json::to_vec(&reduced).unwrap().len();
        eprintln!(
            "synthetic buckets={buckets} full_reply_bytes={full_bytes} digest_guard_reply_bytes={reduced_bytes}"
        );
        assert_eq!(full.inventory.buckets.len(), buckets as usize);
        sizes.push((full_bytes, reduced_bytes));
    }
    assert!(
        sizes[1].0 > sizes[0].0 * 8,
        "negative control: legacy full reply must actually grow"
    );
    assert_eq!(
        sizes[1].1, sizes[0].1,
        "only bucket bytes are omitted, not a timing claim"
    );
}

#[test]
fn signature_requests_and_checkpoint_metadata_survive_reduced_mode() {
    let store = node();
    store
        .connection
        .write()
        .execute_batch(
            "INSERT INTO replica_envelope_holds VALUES('birch',8,'held-hash','unsigned','0');
         INSERT INTO checkpoints(id,cut_unix_ms,state,drop_digest,updated_at_unix_ms)
         VALUES('checkpoint/fixture',0,'trimmed','drop-digest',0);",
        )
        .unwrap();
    let (full, reduced) = equivalent(&store);
    assert_eq!(reduced.signature_requests.len(), 1);
    assert_eq!(reduced.signature_requests, full.signature_requests);
    assert!(reduced.inventory.checkpoint.is_some());
    assert_eq!(reduced.inventory.checkpoint, full.inventory.checkpoint);
}

#[test]
fn rollback_prefix_delete_and_tombstone_changes_use_fresh_surviving_cut() {
    let store = node();
    identity(&store, "alder", 1);
    identity(&store, "alder", 257);
    let (before, _) = equivalent(&store);
    store
        .connection
        .write()
        .execute_batch(
            "SAVEPOINT guard_fixture;
         DELETE FROM replica_envelopes WHERE sequence=1;
         ROLLBACK TO guard_fixture; RELEASE guard_fixture;",
        )
        .unwrap();
    assert_eq!(
        equivalent(&store).0.authority_digest,
        before.authority_digest
    );
    store.connection.write().execute_batch(
        "DELETE FROM replica_envelopes WHERE sequence=1;
         INSERT INTO checkpoint_envelopes VALUES('alder',1,'retained-tombstone',0,'checkpoint/fixture');",
    ).unwrap();
    assert_ne!(
        equivalent(&store).0.authority_digest,
        before.authority_digest
    );
}

#[test]
fn real_local_append_is_sealed_and_seen_by_each_fresh_guard() {
    let store = node();
    let before = equivalent(&store).0.authority_digest;
    store
        .append_claim(&ClaimInput {
            subject: "note/fixture".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("text".into(), serde_json::json!("changed"))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let (full, reduced) = equivalent(&store);
    assert_ne!(reduced.authority_digest, before);
    assert_eq!(reduced.authority_digest, full.authority_digest);
    assert!(
        !full.inventory.buckets.is_empty(),
        "the new local batch was actually sealed"
    );
}

#[test]
fn reopen_and_a_later_write_do_not_reuse_the_previous_guard_result() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fixture.sqlite3");
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    identity(&store, "alder", 1);
    let before = equivalent(&store).0.authority_digest;
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(equivalent(&store).0.authority_digest, before);
    identity(&store, "birch", 1);
    assert_ne!(equivalent(&store).0.authority_digest, before);
}

#[test]
fn snapshot_signature_and_checkpoint_failures_are_not_bypassed() {
    for table in ["batches", "replica_envelope_holds", "checkpoints"] {
        let store = node();
        equivalent(&store);
        store
            .connection
            .write()
            .execute_batch(&format!("DROP TABLE {table}"))
            .unwrap();
        let full = store.export_replication_summary(FLEET).unwrap_err();
        let reduced = store.export_replication_authority_guard(FLEET).unwrap_err();
        assert_eq!(
            format!("{reduced:#}"),
            format!("{full:#}"),
            "failure in {table}"
        );
    }
}
