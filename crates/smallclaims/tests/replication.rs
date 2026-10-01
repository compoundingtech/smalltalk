//! The graph on its own: a runtime that knows no claim kinds and projects nothing still gets a
//! claim log that appends, replicates between members, and stores documents and blobs.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};
use smallclaims::claim::{ClaimInput, ClaimRecord, ReplicaBatch};
use smallclaims::error::Error;
use smallclaims::store::checkpoint::{DropPlan, SealedSet};
use smallclaims::store::runtime::LegacyDigestTable;
use smallclaims::store::{ReplicatedClaimAdmission, Runtime, Store, append_claim_record_tx};

const FLEET: &str = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12";

/// Accepts every claim as it is, keeps no tables of its own, and projects nothing.
struct Plain;

impl Runtime for Plain {
    fn migrate_schema(&self, _connection: &Connection) -> Result<()> {
        Ok(())
    }

    fn create_schema(&self, _connection: &Connection) -> Result<()> {
        Ok(())
    }

    fn open_projections(&self, _transaction: &Transaction<'_>, _shared_memory: bool) -> Result<()> {
        Ok(())
    }

    fn schema_digest(&self) -> String {
        "plain".into()
    }

    fn classify_replicated_claim(
        &self,
        _connection: &Connection,
        _batch: &ReplicaBatch,
        _claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, Error> {
        Ok(ReplicatedClaimAdmission::Valid)
    }

    fn append_claim(&self, store: &Store, input: &ClaimInput) -> Result<(ClaimRecord, bool), Error> {
        let body = json!({ "fields": input.fields, "evidence": input.evidence });
        store
            .connection
            .batched(|transaction| {
                append_claim_record_tx(
                    transaction,
                    &store.origin,
                    &input.subject,
                    &input.kind,
                    input.actor.as_deref(),
                    &body,
                    &[],
                    None,
                )
                .map(|claim| (claim, true))
                .map_err(|error| Error::new("internal", error.to_string()))
            })
            .map_err(|error| Error::new("internal", error))?
    }

    fn apply_repair_tx(
        &self,
        _transaction: &Transaction<'_>,
        _repaired: &str,
        _replacement: &str,
    ) -> Result<()> {
        Ok(())
    }

    fn append_claim_tx(
        &self,
        transaction: &Transaction<'_>,
        origin: &str,
        subject: &str,
        kind: &str,
        actor: Option<&str>,
        body: &Value,
        predecessors: &[String],
        forced_batch: Option<&str>,
    ) -> Result<ClaimRecord> {
        append_claim_record_tx(
            transaction,
            origin,
            subject,
            kind,
            actor,
            body,
            predecessors,
            forced_batch,
        )
    }

    fn project_incremental(
        &self,
        _transaction: &Transaction<'_>,
        _origin: &str,
    ) -> Result<bool, Error> {
        Ok(true)
    }

    fn replay_from_nothing(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        Ok(())
    }

    fn after_projection(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        Ok(())
    }

    fn forget_views(&self) {}

    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] {
        &[
            ("operations", &[]),
            ("blobs", &[]),
            ("documents", &["created_index"]),
        ]
    }

    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] {
        &[]
    }

    fn checkpoint_rules_digest(&self) -> String {
        "plain".into()
    }

    fn plan_checkpoint_drops(&self, _sealed: &SealedSet) -> DropPlan {
        unimplemented!("the plain runtime has no checkpoint rules")
    }

    fn clear_checkpoint_projections(&self, _transaction: &Transaction<'_>) -> Result<()> {
        Ok(())
    }

    fn replay_checkpoint_projections(&self, _transaction: &Transaction<'_>) -> Result<()> {
        Ok(())
    }

    fn checkpoint_subject_answers(
        &self,
        _connection: &Connection,
        _subject: &str,
        _cut: u128,
    ) -> Result<Value> {
        Ok(Value::Null)
    }
}

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
fn a_claim_whose_content_does_not_match_its_id_is_never_admitted() {
    let ada = node("ada-laptop");
    let grace = node("grace-desktop");
    note(&ada, "note/plans", "the original");
    let mut exchange = ada
        .export_replication_exchange(FLEET, &grace.replication_inventory().unwrap())
        .unwrap();
    // Swap the envelope's payload for one that carries a different body under the same claim ID.
    let envelope = exchange.envelopes.first_mut().unwrap();
    envelope.payload = envelope.payload.replace('A', "B");
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
