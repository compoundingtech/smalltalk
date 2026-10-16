use smallclaims::{
    claim::ClaimInput,
    replication::ReplicationInventory,
    store::{Store, runtime::Plain},
};
use std::{collections::BTreeMap, sync::Arc};
const FLEET: &str = "sample-fleet";
fn node(name: &str) -> Store {
    Store::open_memory(name, Arc::new(Plain)).unwrap()
}
fn status(store: &Store) -> smallclaims::replication::ReplicationStatus {
    store
        .replication_status_sealed(true, Some(FLEET), &["alder".into()])
        .unwrap()
}
#[test]
fn an_older_advertised_inventory_is_pending_until_an_aligned_exchange_names_the_difference() {
    let source = node("alder");
    let target = node("birch");
    let mut summary = source.export_replication_summary(FLEET).unwrap();
    summary
        .projection_digests
        .insert("documents".into(), "different".into());
    let aligned_inventory = summary.inventory.digest.clone();
    summary.inventory.digest = "older inventory".into();
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    let peer = &view.peers[0];
    assert_eq!(
        peer.authority_digest.as_deref(),
        Some(view.authority_digest.as_str())
    );
    assert_eq!(peer.status, "up");
    assert_ne!(
        peer.graph_digest.as_deref(),
        Some(view.graph_digest.as_str())
    );
    assert!(peer.differing_tables.is_empty());
    assert!(
        peer.projection_comparison_waiting,
        "comparison was reported ready without an aligned inventory: {peer:?}"
    );
    summary.inventory.digest = aligned_inventory;
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(!view.peers[0].projection_comparison_waiting);
    assert_eq!(view.peers[0].differing_tables, vec!["documents"]);
    summary.projection_digests.clear();
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(view.peers[0].projection_digests.is_empty());
    assert!(!view.peers[0].projection_comparison_waiting);
}
#[test]
fn local_claims_wait_until_their_envelopes_and_peer_comparison_are_current() {
    let source = node("alder");
    let target = node("birch");
    let exchange = source
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    target
        .receive_replication_exchange("alder", FLEET, &exchange)
        .unwrap();
    assert!(!status(&target).peers[0].projection_comparison_waiting);
    target
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
    let view = status(&target);
    assert!(view.peers[0].differing_tables.is_empty());
    assert!(
        view.peers[0].projection_comparison_waiting,
        "unsealed local claims were reported comparable"
    );
}
#[test]
fn a_difference_found_at_an_aligned_exchange_is_not_named_once_the_inventory_moves_on() {
    let source = node("alder");
    let target = node("birch");
    let other = node("cedar");
    let mut summary = source.export_replication_summary(FLEET).unwrap();
    summary
        .projection_digests
        .insert("documents".into(), "different".into());
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(!view.peers[0].projection_comparison_waiting);
    assert_eq!(view.peers[0].differing_tables, vec!["documents"]);
    // Another peer delivers an envelope, so alder's stored inventory is now older than this
    // node's. Whether the table still differs is not known, and none is named.
    other
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
    let from_other = other
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    target
        .receive_replication_exchange("cedar", FLEET, &from_other)
        .unwrap();
    let view = status(&target);
    let peer = &view.peers[0];
    assert!(peer.projection_comparison_waiting);
    assert!(peer.differing_tables.is_empty());
    assert_eq!(peer.projection_graphs_equal, None);
    assert!(
        peer.projection_comparison_waiting_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("documents")),
        "{peer:?}"
    );
}
#[test]
fn an_equal_comparison_stays_equal_while_the_inventory_moves_on() {
    let source = node("alder");
    let target = node("birch");
    let other = node("cedar");
    let summary = source.export_replication_summary(FLEET).unwrap();
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let before = status(&target).peers[0].clone();
    assert!(!before.projection_comparison_waiting);
    assert_eq!(before.projection_graphs_equal, Some(true));
    other
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
    let from_other = other
        .export_replication_exchange(FLEET, &ReplicationInventory::default())
        .unwrap();
    target
        .receive_replication_exchange("cedar", FLEET, &from_other)
        .unwrap();
    let after = status(&target).peers[0].clone();
    assert!(!after.projection_comparison_waiting, "{after:?}");
    assert_eq!(after.projection_graphs_equal, Some(true));
    assert!(after.differing_tables.is_empty());
    assert!(after.projection_compared_at_unix_ms.is_some());
}

#[test]
fn aligned_comparison_excludes_different_definitions_and_expires_when_the_definition_changes() {
    let source = node("alder");
    let target = node("birch");
    let mut summary = source.export_replication_summary(FLEET).unwrap();
    summary.projection_registry_digest = Some(summary.schema_digest.clone());
    summary.schema_digest = "versioned-layout".into();
    summary.projection_versions.insert("documents".into(), 2);
    summary
        .projection_digests
        .insert("documents".into(), "different definition".into());
    let aligned = summary.inventory.digest.clone();
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(!view.peers[0].projection_comparison_waiting);
    assert!(view.peers[0].differing_tables.is_empty());
    assert_eq!(
        view.peers[0].incompatible_projection_tables,
        vec!["documents"]
    );
    summary.inventory.digest = "older inventory".into();
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(!view.peers[0].projection_comparison_waiting);
    assert_eq!(view.peers[0].projection_graphs_equal, Some(true));
    assert!(view.peers[0].differing_tables.is_empty());
    // An older aligned comparison cannot certify a newly advertised definition.
    summary.projection_versions.insert("documents".into(), 3);
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(view.peers[0].projection_comparison_waiting);
    assert_eq!(view.peers[0].projection_graphs_equal, None);
    summary.inventory.digest = aligned;
    target
        .receive_replication_exchange("alder", FLEET, &summary)
        .unwrap();
    let view = status(&target);
    assert!(!view.peers[0].projection_comparison_waiting);
    assert!(view.peers[0].differing_tables.is_empty());
}
