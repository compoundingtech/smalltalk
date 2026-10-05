use smallclaims::{
    claim::ClaimInput,
    replication::ReplicationInventory,
    store::{PEER_QUIET_EXCHANGE_MS, Store, clock_snapshot, now_ms, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};

#[test]
fn a_failed_exchange_makes_last_seen_and_sync_staleness_agree_and_recovery_clears_it() {
    let _clock = clock_snapshot();
    let now = now_ms();
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    // The peer row stamps receipt before inventory comparison completes. A slow comparison
    // can leave the measurement newer than that stamp when the next exchange fails.
    store
        .connection
        .write()
        .execute(
            "INSERT INTO replication_peers(peer,status,last_success_at_unix_ms,updated_at_unix_ms)
         VALUES('birch','up',?1,?1)",
            [(now - PEER_QUIET_EXCHANGE_MS).to_string()],
        )
        .unwrap();
    store
        .replication_sync
        .lock()
        .unwrap()
        .entry("birch".into())
        .or_default()
        .observe(0, Some((0, 0)), now);
    let peers = ["birch".to_string()];
    let status = store
        .replication_status(true, Some("sample-fleet"), &peers)
        .unwrap();
    assert_eq!(status.peers[0].status, "up");
    assert!(!status.peers[0].sync.as_ref().unwrap().stale);
    for _ in 0..20 {
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
    }
    store
        .record_peer_failure("birch", "down", "exchange before headers timed out")
        .unwrap();
    let status = store
        .replication_status(true, Some("sample-fleet"), &peers)
        .unwrap();
    let peer = &status.peers[0];
    assert_eq!(peer.status, "last-seen");
    assert!(peer.last_failure_at_unix_ms.is_some());
    assert!(
        peer.sync.as_ref().unwrap().stale,
        "failed peer had a fresh measurement: {peer:?}"
    );
    assert!(peer.sync.as_ref().unwrap().added_since_measured_envelopes >= 20);
    let source = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    let exchange = source
        .export_replication_exchange("sample-fleet", &ReplicationInventory::default())
        .unwrap();
    store
        .receive_replication_exchange("birch", "sample-fleet", &exchange)
        .unwrap();
    let status = store
        .replication_status(true, Some("sample-fleet"), &peers)
        .unwrap();
    assert_eq!(status.peers[0].status, "up");
    assert!(status.peers[0].last_failure_at_unix_ms.is_none());
    assert!(!status.peers[0].sync.as_ref().unwrap().stale);
}
