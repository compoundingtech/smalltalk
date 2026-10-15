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
    assert!(
        status.peers[0].last_successful_exchange.is_none(),
        "inbound-only traffic has no local carrier evidence"
    );
    assert!(status.peers[0].last_failure_at_unix_ms.is_none());
    assert!(!status.peers[0].sync.as_ref().unwrap().stale);
}

#[test]
fn exchange_carrier_records_are_independent_local_and_cleared_on_restart() {
    use smallclaims::replication::{ReplicationCarrier, ReplicationWorkerStatus};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("alder.sqlite3");
    let source = Store::open_memory("birch", Arc::new(Plain)).unwrap();
    let exchange = source.export_replication_summary("sample-fleet").unwrap();
    let peers = ["birch".to_string()];
    let record;
    {
        let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
        let before = store.index().unwrap();
        store
            .record_peer_failure_via(
                "birch",
                "refused",
                "service grant refused",
                Some(ReplicationCarrier::Fabric),
            )
            .unwrap();
        assert_eq!(
            store.index().unwrap(),
            before,
            "local refusal evidence adds no claims"
        );
        store
            .receive_replication_exchange_via(
                "birch",
                "sample-fleet",
                &exchange,
                true,
                ReplicationCarrier::Loopback,
            )
            .unwrap();
        let status = store
            .replication_status(true, Some("sample-fleet"), &peers)
            .unwrap();
        let peer = &status.peers[0];
        assert_eq!(
            peer.last_successful_exchange.as_ref().unwrap().carrier,
            ReplicationCarrier::Loopback
        );
        assert_eq!(
            peer.last_successful_exchange.as_ref().unwrap().at_unix_ms,
            peer.last_success_at_unix_ms.unwrap()
        );
        record = peer.last_failed_exchange.clone().unwrap();
        assert_eq!(record.carrier, ReplicationCarrier::Fabric);
        assert_eq!(record.fabric_route.as_deref(), Some("unknown"));
        assert!(peer.refusal_reason.is_none());
        assert!(peer.last_error.is_none());
        let before = store.index().unwrap();
        store.record_replication_worker(
            "birch",
            ReplicationWorkerStatus {
                phase: "idle".into(),
                ..Default::default()
            },
        );
        // A liveness failure/probe lacks exchange evidence and cannot relabel an earlier attempt.
        store
            .record_peer_failure_via("birch", "down", "probe failed", None)
            .unwrap();
        assert_eq!(
            store.replication_peer_transport("birch").1.as_ref(),
            Some(&record)
        );
        assert_eq!(store.index().unwrap(), before);
        store
            .receive_replication_exchange_via(
                "birch",
                "sample-fleet",
                &exchange,
                true,
                ReplicationCarrier::Fabric,
            )
            .unwrap();
        let outbound = store.replication_peer_transport("birch").0.unwrap();
        assert_eq!(outbound.carrier, ReplicationCarrier::Fabric);
        assert_eq!(outbound.fabric_route.as_deref(), Some("unknown"));
        // Inbound success updates reachability, preserving the original outbound evidence/time.
        store
            .receive_replication_exchange("birch", "sample-fleet", &exchange)
            .unwrap();
        let (success, failure) = store.replication_peer_transport("birch");
        assert_eq!(success, Some(outbound.clone()));
        assert!(
            store
                .replication_peer_last_success("birch")
                .unwrap()
                .unwrap()
                >= outbound.at_unix_ms
        );
        assert_eq!(failure, Some(record.clone()));
        // A stale completion cannot rewrite a newer record.
        store.record_replication_exchange_transport(
            "birch",
            false,
            record.at_unix_ms.saturating_sub(1),
            ReplicationCarrier::Tailscale,
        );
        assert_eq!(
            store.replication_peer_transport("birch").1,
            Some(record.clone())
        );
        // Evidence never travels in the replicated payload.
        let exported =
            serde_json::to_value(store.export_replication_summary("sample-fleet").unwrap())
                .unwrap();
        assert!(exported.get("last_successful_exchange").is_none());
        assert!(exported.get("carrier").is_none());
    }
    let store = Store::open(&path, "alder", Arc::new(Plain)).unwrap();
    let status = store
        .replication_status(true, Some("sample-fleet"), &peers)
        .unwrap();
    let peer = &status.peers[0];
    assert!(
        peer.last_success_at_unix_ms.is_some(),
        "durable reachability is preserved"
    );
    assert!(peer.last_successful_exchange.is_none());
    assert!(peer.last_failed_exchange.is_none());
    let json = serde_json::to_value(peer).unwrap();
    assert!(json.get("last_successful_exchange").is_none());
    assert!(json.get("last_failed_exchange").is_none());
}

#[test]
fn failures_replace_only_the_failed_record_and_do_not_clear_success() {
    use smallclaims::replication::ReplicationCarrier;
    let store = Store::open_memory("alder", Arc::new(Plain)).unwrap();
    store.record_replication_exchange_transport("birch", true, 10, ReplicationCarrier::Tailscale);
    store.record_replication_exchange_transport("birch", false, 11, ReplicationCarrier::Fabric);
    store.record_replication_exchange_transport("birch", false, 12, ReplicationCarrier::Loopback);
    let (success, failure) = store.replication_peer_transport("birch");
    assert_eq!(success.unwrap().carrier, ReplicationCarrier::Tailscale);
    assert_eq!(failure.unwrap().carrier, ReplicationCarrier::Loopback);
}
