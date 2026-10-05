use smallclaims::{
    claim::ClaimInput,
    store::{Store, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn newly_sealed_records_do_not_store_a_second_claim_copy() {
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
    let (records, raw_bytes): (usize, usize) = store
        .connection
        .write()
        .query_row(
            "SELECT COUNT(*),SUM(length(raw)) FROM replica_records",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(records, 1);
    assert_eq!(
        raw_bytes, 0,
        "replica records still store a second claim copy"
    );
}
