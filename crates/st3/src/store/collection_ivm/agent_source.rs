//! Exact SQL capture descriptors for the agent card source manifest.
//! This list alone does not attest complete producer/authority/deadline coverage or Ready.
pub mod boundary;
pub mod canonical;
pub mod clock;
pub mod extract;
pub(crate) mod row_changes;
pub(crate) mod service;
mod work_progress;
pub mod shadow;

use super::Table;
use anyhow::Result;
use rusqlite::Transaction;

pub const CAPTURE_FINGERPRINT: &str = "st3.agent-card.capture.v5;recursive-delete-v1;declared-sql-inputs-v1;delivery-producer-global-v1;namespace-canonical-v1;canonical-time-text-v1;clock-v2-captured-snapshot-index;unfiltered-verdict-admission-v1";
pub const SOURCE: &str = "st3.agent-card-source.v1";

/// Additional lifecycle/local producers must expand this manifest before source attestation.
pub const TABLES: &[Table] = &[
    Table {
        name: "claims",
        columns: &[
            "store_index",
            "id",
            "batch_id",
            "subject",
            "kind",
            "origin",
            "actor",
            "body",
            "predecessors",
            "accepted_at_unix_ms",
        ],
        key: &["store_index"],
    },
    Table {
        name: "batches",
        columns: &[
            "id",
            "origin",
            "replica_sequence",
            "previous_hash",
            "hash",
            "accepted_at_unix_ms",
        ],
        key: &["id"],
    },
    Table {
        name: "operations",
        columns: &["id", "request_digest", "canonical_claim_id", "state"],
        key: &["id"],
    },
    Table {
        name: "replica_records",
        columns: &[
            "record_ref",
            "writer",
            "sequence",
            "envelope_hash",
            "position",
            "raw",
            "state",
            "claim_id",
            "subject_hint",
            "kind_hint",
            "error_code",
            "error_message",
            "replacement_claim_id",
            "updated_at_unix_ms",
        ],
        key: &["record_ref"],
    },
    Table {
        name: "checkpoint_claims",
        columns: &[
            "id",
            "writer",
            "sequence",
            "envelope_hash",
            "subject",
            "kind",
            "actor",
            "predecessors",
            "operation_id",
            "request_digest",
            "accepted_at_unix_ms",
            "checkpoint",
        ],
        key: &["id"],
    },
    Table {
        name: "checkpoints",
        columns: &[
            "id",
            "cut_unix_ms",
            "state",
            "seal_rowid",
            "sealed_digest",
            "drop_digest",
            "graph_digest",
            "detail",
            "updated_at_unix_ms",
        ],
        key: &["id"],
    },
    Table {
        name: "desired",
        columns: &[
            "subject",
            "kind",
            "revision",
            "claim_id",
            "body",
            "member",
            "owner_run",
            "owner_generation",
            "owner_step",
        ],
        key: &["subject"],
    },
    Table {
        name: "mission_runs",
        columns: &[
            "id",
            "mission_id",
            "initial_revision",
            "current_generation_id",
            "root_revision",
            "root_run_id",
            "parent_step_run",
            "workspace",
            "requester",
            "inputs",
            "mode",
            "status",
            "phase",
            "created_at_unix_ms",
            "updated_at_unix_ms",
        ],
        key: &["id"],
    },
    Table {
        name: "run_generations",
        columns: &[
            "id",
            "run_id",
            "revision",
            "predecessor_id",
            "status",
            "actor",
            "reason",
            "created_at_unix_ms",
            "updated_at_unix_ms",
        ],
        key: &["id"],
    },
    Table {
        name: "step_runs",
        columns: &[
            "subject",
            "run_id",
            "generation_id",
            "step_path",
            "definition_hash",
            "status",
            "attempt",
            "assignee",
            "available_to",
            "agentless",
            "title",
            "goals",
            "worker_reported",
            "lease_owner",
            "lease_incarnation",
            "lease_expires_at_unix_ms",
            "blocked_reason",
            "not_before_unix_ms",
            "activated_at_unix_ms",
            "readiness_epoch",
            "created_at_unix_ms",
            "updated_at_unix_ms",
            "constraints",
        ],
        key: &["subject"],
    },
    Table {
        name: "local_work_lease_renewals",
        columns: &[
            "subject",
            "attempt",
            "lease_owner",
            "lease_incarnation",
            "lease_expires_at_unix_ms",
            "updated_at_unix_ms",
        ],
        key: &["subject"],
    },
    Table {
        name: "local_observations",
        columns: &[
            "id",
            "after_store_index",
            "subject",
            "kind",
            "actor",
            "body",
            "dedupe_key",
            "request_digest",
            "observed_at_unix_ms",
        ],
        key: &["id"],
    },
    Table {
        name: "local_mailbox_owners",
        columns: &["subject", "component", "incarnation", "epoch"],
        key: &["subject", "component"],
    },
    Table {
        name: "local_agent_card_clock",
        columns: &["singleton", "at_ms", "revision", "reason", "snapshot_index"],
        key: &["singleton"],
    },
    Table {
        name: "local_agent_delivery_presence",
        columns: &[
            "recipient",
            "driver",
            "producer_epoch",
            "producer_revision",
            "state",
            "assessment",
            "certificate",
            "evaluation_time_ms",
            "next_deadline_ms",
            "error",
        ],
        key: &["recipient", "driver"],
    },
    Table {
        name: "local_mailbox_bindings",
        columns: &["token", "subject", "component", "incarnation", "epoch"],
        key: &["token"],
    },
];

/// Current public cards select admitted claims without consulting mutable verdict caches.
/// A policy change must replace this binding and expand the source manifest before publication.
/// Bind both the claim vocabulary and shared projection schema to this exact eligibility domain.
pub fn capture_fingerprint() -> String {
    format!(
        "{CAPTURE_FINGERPRINT};schema={}",
        crate::store::runtime::compatibility_digest(&st3_schema::registry().digest())
    )
}

/// Receiver identity participates in local observation and delivery selection. Production
/// installation must bind this value; changing it requires a separate source lifecycle.
pub fn capture_fingerprint_for(receiver: &str) -> Result<String> {
    anyhow::ensure!(
        !receiver.is_empty() && receiver.len() <= 1024 && !receiver.bytes().any(|b| b == 0),
        "invalid agent source receiver identity"
    );
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{};receiver-sha256={}",
        capture_fingerprint(),
        hex::encode(Sha256::digest(receiver.as_bytes()))
    ))
}

/// Mandatory production read gate, independent of Views readiness and producer certificates.
/// Raw DDL changes invalidate the exact installed source schema before another writer runs.
pub(crate) fn receiver_readable(connection: &rusqlite::Connection, receiver: &str) -> Result<bool> {
    use rusqlite::OptionalExtension as _;
    if !super::scope::readable(connection)?
        || super::status(connection)?.fingerprint != capture_fingerprint_for(receiver)?
    {
        return Ok(false);
    }
    let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='st3_agent_source_service')",[],|r|r.get(0))?;
    if !exists {
        return Ok(false);
    }
    let current: u64 = connection.query_row("PRAGMA schema_version", [], |r| r.get(0))?;
    let expected: Option<u64> = connection
        .query_row(
            "SELECT schema_version FROM st3_agent_source_service WHERE singleton=1 AND receiver=?1",
            [receiver],
            |r| r.get(0),
        )
        .optional()?;
    Ok(expected == Some(current))
}

pub fn install_capture_for(tx: &Transaction<'_>, receiver: &str, epoch: u64) -> Result<()> {
    super::delivery::create_schema(tx)?;
    clock::create_schema(tx)?;
    super::install_recursive(tx, TABLES, &capture_fingerprint_for(receiver)?, epoch)
}

/// Install input capture only; paired hooks must be attached before explicit source registration.
pub fn install_capture(tx: &Transaction<'_>, epoch: u64) -> Result<()> {
    super::delivery::create_schema(tx)?;
    clock::create_schema(tx)?;
    let fingerprint = capture_fingerprint();
    super::install_recursive(tx, TABLES, &fingerprint, epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_agent_source_schema_and_committed_claim_rows_match_descriptors() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        store
            .connection
            .batched(|tx| install_capture(tx, 1))
            .unwrap()
            .unwrap();
        let claim = store
            .append_claim(&crate::model::ClaimInput {
                subject: "agent/capture-probe".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: std::collections::BTreeMap::from([
                    ("runtime_id".into(), serde_json::json!("capture-probe")),
                    ("status".into(), serde_json::json!("running")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let connection = store.readers.get();
        let capture = super::super::page(&connection, 128).unwrap();
        let captured = capture.iter().find(|row| row.table == "claims").unwrap();
        let row = captured.replacements[0].new.as_ref().unwrap();
        assert_eq!(row["id"], claim.id);
        assert_eq!(row["store_index"], claim.store_index);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(row["body"].as_str().unwrap()).unwrap(),
            claim.body
        );
        assert!(captured.replacements[0].old.is_none());
        assert!(!super::super::scope::readable(&connection).unwrap());
        assert!(store.ivm_views().is_none());
    }
}

#[cfg(test)]
mod receiver_tests {
    use super::*;
    #[test]
    fn changed_receiver_cannot_reuse_native_capture_binding() {
        let store = crate::store::Store::open_memory("alder").unwrap();
        store
            .connection
            .batched(|tx| install_capture_for(tx, "alder", 1))
            .unwrap()
            .unwrap();
        let before = super::super::status(&store.readers.get()).unwrap();
        assert_eq!(
            before.fingerprint,
            capture_fingerprint_for("alder").unwrap()
        );
        store
            .connection
            .batched(|tx| install_capture_for(tx, "briar", 1))
            .unwrap()
            .unwrap();
        let after = super::super::status(&store.readers.get()).unwrap();
        assert_eq!(after.fingerprint, before.fingerprint);
        assert!(after.gap.is_some());
    }
}
