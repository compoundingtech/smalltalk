use rusqlite::types::Value;
use smallclaims::{
    claim::ClaimInput,
    replication::ReplicationInventory,
    store::{Store, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

fn note(store: &Store, n: usize) {
    store
        .append_claim(&ClaimInput {
            subject: "note/sample".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("text".into(), serde_json::json!(format!("note {n}")))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some(format!("note-{n}")),
        })
        .unwrap();
}
fn raw(store: &Store) -> BTreeMap<String, Value> {
    store
        .replica_records(false)
        .unwrap()
        .into_iter()
        .map(|record| {
            let value = store
                .replica_record_raw(&record.record_ref)
                .unwrap()
                .unwrap();
            (record.record_ref, value)
        })
        .collect()
}
fn indexes(store: &Store) -> Vec<(String, Option<String>)> {
    store
        .connection
        .write()
        .prepare("SELECT name,sql FROM sqlite_master WHERE type='index' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}
fn make_legacy(store: &Store) -> BTreeMap<String, Value> {
    let original = raw(store);
    let connection = store.connection.write();
    for (id, value) in &original {
        connection.execute("UPDATE replica_records SET raw=?2,raw_offset=NULL,raw_length=NULL,raw_mode=NULL WHERE record_ref=?1", rusqlite::params![id,value]).unwrap();
    }
    connection
        .execute_batch(
            "PRAGMA user_version=16; DELETE FROM meta WHERE key='replica_record_offset_cursor';",
        )
        .unwrap();
    original
}
#[test]
fn legacy_pages_resume_and_keep_raw_wire_authority_indexes_and_peer_healing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.sqlite3");
    let source = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    for n in 0..150 {
        note(&source, n);
    }
    let original_exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    let original_wire = serde_json::to_vec(&original_exchange).unwrap();
    let original_indexes = indexes(&source);
    let original_raw = make_legacy(&source);
    let authority = source
        .replication_snapshot()
        .unwrap()
        .authority_digest
        .clone();
    let behind = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    let mut first = original_exchange.clone();
    first.envelopes.truncate(8);
    behind
        .receive_replication_exchange("alder", "sample-fleet", &first)
        .unwrap();
    behind.validate_replication_backlog().unwrap();
    drop(source);
    let source = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(raw(&source), original_raw);
    let page = source.convert_record_offsets().unwrap();
    assert!(!page.done && page.scanned > 0 && page.scanned <= 64);
    assert_eq!(page.converted, page.scanned);
    drop(source);
    let source = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    while !source.convert_record_offsets().unwrap().done {}
    assert_eq!(raw(&source), original_raw);
    assert_eq!(indexes(&source), original_indexes);
    assert_eq!(
        source.replication_snapshot().unwrap().authority_digest,
        authority
    );
    assert_eq!(
        serde_json::to_vec(
            &source
                .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
                .unwrap()
        )
        .unwrap(),
        original_wire
    );
    let fresh = Store::open_memory("cedar", Arc::new(Plain)).unwrap();
    for peer in [&behind, &fresh] {
        let exchange = source
            .export_replication_exchange("sample-fleet", &peer.replication_inventory().unwrap())
            .unwrap();
        peer.receive_replication_exchange("alder", "sample-fleet", &exchange)
            .unwrap();
        peer.validate_replication_backlog().unwrap();
        peer.readmit_envelopes("alder", &exchange.envelopes)
            .unwrap();
        peer.project_replication_backlog().unwrap();
        assert_eq!(
            peer.replication_snapshot().unwrap().authority_digest,
            authority
        );
        assert_eq!(raw(peer), original_raw);
    }
}
#[test]
fn forensic_text_and_nonduplicate_orphan_raw_survive_conversion() {
    let source = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    note(&source, 0);
    let mut exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    exchange.envelopes[0].payload = "%%% invalid base64".into();
    let target = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    target
        .receive_replication_exchange("alder", "sample-fleet", &exchange)
        .unwrap();
    target.validate_replication_backlog().unwrap();
    let original = make_legacy(&target);
    target.connection.write().execute_batch(
        "INSERT INTO replica_records(record_ref,writer,sequence,envelope_hash,position,raw,state,updated_at_unix_ms)
         VALUES('orphan/sample','cedar',1,'absent',0,X'0001ff','invalid','0');"
    ).unwrap();
    while !target.convert_record_offsets().unwrap().done {}
    for (id, value) in original {
        assert_eq!(target.replica_record_raw(&id).unwrap(), Some(value));
    }
    assert_eq!(
        target.replica_record_raw("orphan/sample").unwrap(),
        Some(Value::Blob(vec![0, 1, 255]))
    );
}
#[test]
fn failed_pointer_page_rolls_back_cursor_and_raw_then_retries() {
    let source = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    for n in 0..3 {
        note(&source, n);
    }
    source.replication_snapshot().unwrap();
    let original = make_legacy(&source);
    source
        .connection
        .write()
        .execute_batch(
            "CREATE TRIGGER fail_sample BEFORE UPDATE OF raw ON replica_records WHEN OLD.rowid=2
         BEGIN SELECT RAISE(ABORT,'sample interruption'); END;",
        )
        .unwrap();
    assert!(source.convert_record_offsets().is_err());
    assert_eq!(raw(&source), original);
    let pointers: usize = source
        .connection
        .write()
        .query_row(
            "SELECT COUNT(*) FROM replica_records WHERE raw_mode IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pointers, 0);
    source
        .connection
        .write()
        .execute_batch("DROP TRIGGER fail_sample;")
        .unwrap();
    while !source.convert_record_offsets().unwrap().done {}
    assert_eq!(raw(&source), original);
}

#[test]
fn blob_bytes_and_first_forensic_representation_survive_readmission() {
    let source = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    let bytes = (0..=255).collect::<Vec<u8>>();
    let hash = source.put_blob(&bytes).unwrap();
    source
        .append_claim(&ClaimInput {
            subject: "note/blob".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::from([("blob_hash".into(), serde_json::json!(hash))]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    let target = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    target
        .receive_replication_exchange("alder", "sample-fleet", &exchange)
        .unwrap();
    let envelope = &exchange.envelopes[0];
    smallclaims::store::record_invalid_replica_envelope(
        &target.connection.write(),
        envelope,
        &smallclaims::error::Error::new("sample-error", "sample interruption"),
    )
    .unwrap();
    let forensic_ref = smallclaims::hash::replica_record_ref(
        &envelope.writer,
        envelope.sequence,
        &envelope.hash,
        0,
    );
    let forensic = Value::Text(envelope.payload.base64());
    assert_eq!(
        target.replica_record_raw(&forensic_ref).unwrap(),
        Some(forensic.clone())
    );
    target
        .readmit_envelopes("alder", &exchange.envelopes)
        .unwrap();
    assert_eq!(
        target.replica_record_raw(&forensic_ref).unwrap(),
        Some(forensic.clone())
    );
    let blob_ref: String = target
        .connection
        .write()
        .query_row(
            "SELECT record_ref FROM replica_records WHERE kind_hint='blob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        target.replica_record_raw(&blob_ref).unwrap(),
        Some(Value::Blob(bytes))
    );
    let original = make_legacy(&target);
    while !target.convert_record_offsets().unwrap().done {}
    assert_eq!(raw(&target), original);
    assert_eq!(
        target.replica_record_raw(&forensic_ref).unwrap(),
        Some(forensic)
    );
}

#[test]
fn healing_a_corrupt_or_malformed_envelope_keeps_its_original_forensic_bytes() {
    let source = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    note(&source, 0);
    let good = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    for payload in [
        vec![0xde, 0xad, 0xbe, 0xef].into(),
        "%%% invalid base64".into(),
    ] {
        let target = Store::open_memory("birch", Arc::new(Plain)).unwrap();
        let mut corrupt = good.clone();
        corrupt.envelopes[0].payload = payload;
        target
            .receive_replication_exchange("alder", "sample-fleet", &corrupt)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        let original = make_legacy(&target);
        while !target.convert_record_offsets().unwrap().done {}
        target.readmit_envelopes("alder", &good.envelopes).unwrap();
        assert_eq!(
            raw(&target),
            original,
            "healing changed the first forensic representation"
        );
    }
}
