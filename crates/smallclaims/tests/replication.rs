//! The graph on its own: a runtime that knows no claim kinds and projects nothing still gets a
//! claim log that appends, replicates between members, and stores documents and blobs.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use smallclaims::claim::{ClaimInput, ClaimRecord};
use smallclaims::store::Store;
use smallclaims::store::runtime::Plain;

const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";

fn node(name: &str) -> Store {
    let store = Store::open_memory(name, Arc::new(Plain)).unwrap();
    store.bind_fleet(FLEET).unwrap();
    store
}

fn note(store: &Store, subject: &str, text: &str) -> ClaimRecord {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "example.note".into(),
            actor: Some("person/ada".into()),
            fields: BTreeMap::from([("text".into(), Value::String(text.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap()
}

/// Send `from`'s envelopes that `to` lacks, and admit and project them on `to`.
fn sync(from: &Store, from_name: &str, to: &Store) -> usize {
    let exchange = from
        .export_replication_exchange(FLEET, &to.replication_inventory().unwrap())
        .unwrap();
    let receipt = to
        .receive_replication_exchange(from_name, FLEET, &exchange)
        .unwrap();
    to.validate_replication_backlog().unwrap();
    to.apply_replication_repairs().unwrap();
    assert!(to.project_replication_backlog().unwrap());
    receipt.received
}

#[test]
fn claims_replicate_between_members_and_their_logs_agree() {
    let ada = node("ada-laptop");
    let grace = node("grace-desktop");
    let first = note(&ada, "note/plans", "start with the log");
    let second = note(&grace, "note/plans", "then the replicas");

    assert_eq!(sync(&ada, "ada-laptop", &grace), 1);
    assert_eq!(sync(&grace, "grace-desktop", &ada), 1);

    for store in [&ada, &grace] {
        let ids = store
            .claims_for("note/plans", None)
            .unwrap()
            .into_iter()
            .map(|claim| claim.id)
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&first.id) && ids.contains(&second.id));
    }
    let digests = [&ada, &grace].map(|store| {
        store
            .replication_status(true, Some(FLEET), &[])
            .unwrap()
            .authority_digest
    });
    assert_eq!(digests[0], digests[1]);
    // Nothing is left to send either way.
    assert_eq!(sync(&ada, "ada-laptop", &grace), 0);
    assert_eq!(sync(&grace, "grace-desktop", &ada), 0);
}

#[test]
fn full_inventory_fallback_requires_a_complete_digest_and_bounds_payloads() {
    let source = node("birch");
    for index in 0..6 {
        note(&source, &format!("note/{index}"), "a live payload");
    }
    let mut remote = source.replication_inventory().unwrap();
    remote.envelopes.truncate(4);
    remote.digest = smallclaims::store::replication_inventory_digest(&remote.envelopes);
    remote.accepts = Some(1);
    let answer = source.export_replication_exchange(FLEET, &remote).unwrap();
    assert_eq!(
        answer.envelopes.len(),
        1,
        "the full path retains the peer's payload page limit"
    );
    assert_eq!(answer.inventory.envelopes.len(), 6);

    remote.envelopes.truncate(3);
    assert!(
        source
            .export_replication_exchange(FLEET, &remote)
            .unwrap()
            .envelopes
            .is_empty(),
        "a truncated listing cannot prove which identities the peer lacks"
    );
    remote.envelopes.clear();
    let answer = source.export_replication_exchange(FLEET, &remote).unwrap();
    assert!(
        answer.envelopes.is_empty(),
        "a bare, nonempty digest cannot prove an empty inventory"
    );
    assert_eq!(
        answer.inventory.envelopes.len(),
        6,
        "the peer first receives the full identity proof"
    );
}

#[test]
fn a_claim_whose_content_does_not_match_its_id_is_never_admitted() {
    let ada = node("ada-laptop");
    let grace = node("grace-desktop");
    note(&ada, "note/plans", "the original");
    let mut exchange = ada
        .export_replication_exchange(FLEET, &grace.replication_inventory().unwrap())
        .unwrap();
    // Swap the envelope's payload for one that carries a different body under the same claim ID.
    let envelope = exchange.envelopes.first_mut().unwrap();
    envelope.payload = envelope.payload.base64().replace('A', "B").into();
    grace
        .receive_replication_exchange("ada-laptop", FLEET, &exchange)
        .unwrap();
    grace.validate_replication_backlog().unwrap();
    assert!(grace.claims_for("note/plans", None).unwrap().is_empty());
}

#[test]
fn a_documents_bytes_and_binding_replicate_by_content() {
    let ada = node("ada-laptop");
    let grace = node("grace-desktop");
    let version = ada
        .put_document("doc/example/plan", b"# Plan\n", &None, "plan-v1")
        .unwrap();
    assert_eq!(
        ada.get_document("doc/example/plan", &version.hash)
            .unwrap()
            .as_deref(),
        Some(b"# Plan\n".as_slice())
    );
    sync(&ada, "ada-laptop", &grace);
    // The binding claim and the bytes it names arrive together. Which version a name resolves
    // to is the runtime's projection, and this runtime projects nothing.
    let bindings = grace.claims_for("doc/example/plan", None).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].kind, "doc.bound");
    assert_eq!(
        grace.get_blob(&version.hash).unwrap().as_deref(),
        Some(b"# Plan\n".as_slice())
    );
}
