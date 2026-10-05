use base64::{Engine as _, engine::general_purpose::STANDARD};
use smallclaims::{
    claim::{ClaimInput, EnvelopePayload},
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

#[test]
fn wire_and_sql_preserve_every_byte_and_malformed_evidence() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    for text in [
        STANDARD.encode((0..=255).collect::<Vec<u8>>()),
        "%%% invalid base64".into(),
    ] {
        let json = serde_json::to_string(&text).unwrap();
        let payload: EnvelopePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&payload).unwrap(), json);
        let legacy: EnvelopePayload = connection
            .query_row("SELECT ?1", [&text], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy, payload);
        let stored: EnvelopePayload = connection
            .query_row("SELECT ?1", [&payload], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, payload);
        assert_eq!(stored.bytes().is_ok(), text != "%%% invalid base64");
    }
}

#[test]
fn interrupted_conversion_reopens_with_identical_authority_and_heals_old_and_new_peers() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.sqlite3");
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    for n in 0..150 {
        note(&store, n);
    }
    let snapshot = store.replication_snapshot().unwrap();
    let old_peer = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    let mut first = store
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    first.envelopes.truncate(10);
    old_peer
        .receive_replication_exchange("alder", "sample-fleet", &first)
        .unwrap();
    old_peer.validate_replication_backlog().unwrap();
    {
        let connection = store.connection.write();
        let rows: Vec<(i64, Vec<u8>)> = connection
            .prepare("SELECT rowid,payload FROM replica_envelopes")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (rowid, bytes) in rows {
            connection
                .execute(
                    "UPDATE replica_envelopes SET payload=?1 WHERE rowid=?2",
                    rusqlite::params![STANDARD.encode(bytes), rowid],
                )
                .unwrap();
        }
        connection.execute_batch("PRAGMA user_version=15;").unwrap();
    }
    let before_indexes = indexes(&store);
    let before = serde_json::to_vec(
        &store
            .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
            .unwrap(),
    )
    .unwrap();
    let authority = store
        .replication_status(true, Some("sample-fleet"), &[])
        .unwrap()
        .authority_digest;
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    assert_eq!(
        serde_json::to_vec(
            &store
                .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
                .unwrap()
        )
        .unwrap(),
        before
    );
    let page = store.convert_envelope_payloads().unwrap();
    assert!(page.scanned > 0 && page.scanned <= 64 && !page.done);
    assert_eq!(page.scanned, page.converted);
    assert_eq!(indexes(&store), before_indexes);
    drop(store);
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    while !store.convert_envelope_payloads().unwrap().done {}
    assert_eq!(
        store
            .replication_status(true, Some("sample-fleet"), &[])
            .unwrap()
            .authority_digest,
        authority
    );
    assert_eq!(
        serde_json::to_vec(
            &store
                .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
                .unwrap()
        )
        .unwrap(),
        before
    );
    assert_eq!(indexes(&store), before_indexes);
    assert_eq!(
        store
            .connection
            .write()
            .query_row(
                "SELECT COUNT(*) FROM replica_envelopes WHERE typeof(payload)='text'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        0
    );
    let fresh_peer = Store::open_memory("cedar", Arc::new(Plain)).unwrap();
    for peer in [&old_peer, &fresh_peer] {
        let exchange = store
            .export_replication_exchange("sample-fleet", &peer.replication_inventory().unwrap())
            .unwrap();
        peer.receive_replication_exchange("alder", "sample-fleet", &exchange)
            .unwrap();
        peer.validate_replication_backlog().unwrap();
        peer.project_replication_backlog().unwrap();
        peer.readmit_envelopes("alder", &exchange.envelopes)
            .unwrap();
        assert_eq!(
            peer.replication_status(true, Some("sample-fleet"), &[])
                .unwrap()
                .authority_digest,
            authority
        );
    }
    assert_eq!(
        store.replication_snapshot().unwrap().inventory.digest,
        snapshot.inventory.digest
    );
}

#[test]
fn malformed_payload_is_admitted_as_residue_and_survives_conversion() {
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
    assert_eq!(target.validate_replication_backlog().unwrap().invalid, 1);
    let page = target.convert_envelope_payloads().unwrap();
    assert_eq!(page.converted, 0);
    let text: String = target
        .connection
        .write()
        .query_row("SELECT payload FROM replica_envelopes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "%%% invalid base64");
    let record = target.replica_records(false).unwrap();
    assert_eq!(
        record[0].error_code.as_deref(),
        Some("invalid-envelope-payload")
    );
}

#[test]
fn a_failed_page_rolls_back_its_bytes_and_cursor_before_retry() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    for n in 0..3 {
        note(&store, n);
    }
    let exchange = store
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    {
        let tx = store.connection.write();
        for envelope in exchange.envelopes {
            tx.execute(
                "UPDATE replica_envelopes SET payload=?1 WHERE envelope_hash=?2",
                rusqlite::params![envelope.payload.base64(), envelope.hash],
            )
            .unwrap();
        }
        tx.execute_batch(
            "CREATE TRIGGER fail_payload_conversion BEFORE UPDATE OF payload ON replica_envelopes
            WHEN OLD.rowid=2 BEGIN SELECT RAISE(ABORT,'injected conversion fault'); END;",
        )
        .unwrap();
    }
    assert!(store.convert_envelope_payloads().is_err());
    {
        let tx = store.connection.write();
        assert_eq!(
            tx.query_row(
                "SELECT COUNT(*) FROM replica_envelopes WHERE typeof(payload)='text'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            3
        );
        assert_eq!(
            tx.query_row(
                "SELECT COUNT(*) FROM meta WHERE key='binary_envelope_payload_cursor'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        tx.execute_batch("DROP TRIGGER fail_payload_conversion")
            .unwrap();
    }
    assert_eq!(store.convert_envelope_payloads().unwrap().converted, 3);
    assert!(store.convert_envelope_payloads().unwrap().done);
}
