use base64::{Engine as _, engine::general_purpose::STANDARD};
use st3::{
    model::{ClaimInput, ReplicationInventory},
    store::Store,
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn upgrade_preserves_signed_backups_retries_and_stream_cursors() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.sqlite3");
    let store = Store::open(&path, "alder").unwrap();
    store.bind_fleet("sample-fleet").unwrap();
    let key = Arc::new(st3::fleet::MemberKey::generate().unwrap().0);
    store.set_member_key(Some(key.clone())).unwrap();
    let input = |n: usize| ClaimInput {
        subject: "daemon/alder".into(),
        kind: "daemon.diagnostic".into(),
        actor: None,
        fields: BTreeMap::from([
            ("severity".into(), serde_json::json!("warning")),
            ("code".into(), serde_json::json!("binary-payload-test")),
            ("reason".into(), serde_json::json!(format!("note-{n}"))),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: Some(format!("note-{n}")),
    };
    // Put one operation beyond the planned seven-day response horizon. Storage conversion
    // must preserve both old and recent retries and their stream cursors.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    store
        .connection
        .batched(|_| {
            smallclaims::store::set_thread_clock(Some(now - 8 * 86_400_000));
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    let old = store.append_claim(&input(0)).unwrap();
    store
        .connection
        .batched(|_| {
            smallclaims::store::set_thread_clock(None);
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    assert!(old.accepted_at_unix_ms <= now - 8 * 86_400_000);
    let recent = store.append_claim(&input(1)).unwrap();
    let events = serde_json::to_value(store.events_after(0, None).unwrap()).unwrap();
    let resumed = serde_json::to_value(store.events_after(old.store_index, None).unwrap()).unwrap();
    let exchange = store
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    for envelope in &exchange.envelopes {
        assert!(st3::fleet::verify_signature(
            key.public(),
            &st3::fleet::envelope_signature_message(
                "sample-fleet",
                &envelope.writer,
                envelope.sequence,
                &envelope.hash,
            ),
            envelope.signature.as_deref().unwrap()
        ));
    }
    let authority = store
        .replication_status(true, Some("sample-fleet"), &[])
        .unwrap()
        .authority_digest;
    let raw_records = store
        .replica_records(false)
        .unwrap()
        .into_iter()
        .map(|record| {
            let raw = store
                .replica_record_raw(&record.record_ref)
                .unwrap()
                .unwrap();
            (record.record_ref, raw)
        })
        .collect::<Vec<_>>();
    {
        let connection = store.connection.write();
        let payloads: Vec<(i64, Vec<u8>)> = connection
            .prepare("SELECT rowid,payload FROM replica_envelopes")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (rowid, bytes) in payloads {
            connection
                .execute(
                    "UPDATE replica_envelopes SET payload=?1 WHERE rowid=?2",
                    rusqlite::params![STANDARD.encode(bytes), rowid],
                )
                .unwrap();
        }
        for (record_ref, raw) in &raw_records {
            connection.execute("UPDATE replica_records SET raw=?2,raw_offset=NULL,raw_length=NULL,raw_mode=NULL WHERE record_ref=?1",
                rusqlite::params![record_ref,raw]).unwrap();
        }
        connection.execute_batch("PRAGMA user_version=15").unwrap();
    }
    drop(store);
    let store = Store::open(&path, "alder").unwrap();
    store.set_member_key(Some(key)).unwrap();
    while !store.convert_envelope_payloads().unwrap().done {}
    while !store.convert_record_offsets().unwrap().done {}
    for (record_ref, raw) in raw_records {
        assert_eq!(store.replica_record_raw(&record_ref).unwrap(), Some(raw));
    }
    assert_eq!(
        store
            .replication_status(true, Some("sample-fleet"), &[])
            .unwrap()
            .authority_digest,
        authority
    );
    assert_eq!(
        serde_json::to_value(
            store
                .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(exchange).unwrap()
    );
    assert_eq!(store.append_claim(&input(0)).unwrap().id, old.id);
    assert_eq!(store.append_claim(&input(1)).unwrap().id, recent.id);
    assert_eq!(
        serde_json::to_value(store.events_after(0, None).unwrap()).unwrap(),
        events
    );
    assert_eq!(
        serde_json::to_value(store.events_after(old.store_index, None).unwrap()).unwrap(),
        resumed
    );
    let archive = root.path().join("history.jsonl");
    let header = store
        .write_backup(&mut std::fs::File::create(&archive).unwrap())
        .unwrap();
    let restored = st3::backup::restore(&archive, &root.path().join("restored.sqlite3")).unwrap();
    assert!(restored.projections_match);
    assert_eq!(restored.log_digest, header.log_digest);
}
