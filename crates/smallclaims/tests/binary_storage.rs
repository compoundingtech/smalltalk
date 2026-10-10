use smallclaims::{
    claim::ClaimInput,
    store::{Store, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn reopening_current_schema_does_not_rewrite_database_header() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("claims.sqlite3");
    drop(Store::open(&path, "alder", Arc::new(Plain)).unwrap());
    let observer = rusqlite::Connection::open(&path).unwrap();
    let before: u32 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    let reopened = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let after: u32 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    assert_eq!(after, before, "reopen made an unnecessary durable write");
    let version: u32 = reopened
        .connection
        .write()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 18);
}

#[test]
fn newly_sealed_envelopes_store_exact_bytes() {
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    store
        .append_claim(&ClaimInput {
            subject: "note/sample".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    store.replication_snapshot().unwrap();
    let binary: bool = store
        .connection
        .write()
        .query_row(
            "SELECT typeof(payload)='blob' FROM replica_envelopes",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(binary, "envelope payload is still stored as base64 text");
}
