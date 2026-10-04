use smallclaims::{
    claim::ClaimInput,
    store::{Store, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

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
