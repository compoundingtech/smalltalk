//! What a runtime built on the graph plugs into the store.
//!
//! The graph knows claims, envelopes, batches, documents and blobs, but not what any claim kind
//! means. A runtime knows its kinds: their schemas, the tables it projects them into, and which of
//! them a checkpoint may drop. The store calls it at each of these seams, inside the store's own
//! transactions, and never the other way around: the runtime may call the graph freely.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};

use super::checkpoint::{DropPlan, SealedSet};
use super::checkpoint_staging::{CheckpointClaimFacts, StagedPlanner};
use super::{ReplicatedClaimAdmission, Store, append_claim_record_tx};
use crate::claim::{ClaimInput, ClaimRecord, ReplicaBatch};
use crate::error::Error;

/// One table of the six-table graph digest older peers still compare: digest label, table,
/// digested columns in order, and row order.
pub type LegacyDigestTable = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static str,
);

/// Whether the healthy frontier can be extended, or why a canonical replay is required.
#[derive(Debug, PartialEq, Eq)]
pub enum IncrementalProjection {
    Projected,
    Replay(&'static str),
    /// The same replay decision, with identifiers from the guard that requested it.
    ReplayWithContext {
        reason: &'static str,
        details: serde_json::Map<String, Value>,
    },
}

/// Work completed inside a replay transaction; it does not advance the committed frontier.
#[derive(Clone, Copy, Debug)]
pub struct ReplayProgress {
    pub phase: &'static str,
    pub processed: Option<u64>,
    pub total: Option<u64>,
}

/// A runtime's claim kinds and projections.
pub trait Runtime: Send + Sync {
    /// Optional process-local invalidation at the single writer's resolution boundary.
    fn writer_observer(&self) -> Option<std::sync::Arc<dyn crate::sqlite::WriterObserver>> {
        None
    }
    /// A completed local response still names live work. Cleanup extends its retry window;
    /// the runtime checks only the bounded candidate, never its complete history.
    fn idempotency_response_in_use(
        &self,
        _connection: &Connection,
        _response: &str,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Migrate an older store's tables before any table is created. The graph has checked that
    /// the schema version is one it supports.
    fn migrate_schema(&self, connection: &Connection) -> Result<()>;

    /// Create the runtime's tables and its indexes on the claim log, once the graph's exist.
    fn create_schema(&self, connection: &Connection) -> Result<()>;

    /// Install a runtime for a store that had no user objects when opening began.
    /// Runtimes may batch fresh DDL here; existing and incomplete stores use create_schema.
    fn create_schema_for_fresh_store(&self, connection: &Connection) -> Result<()> {
        self.create_schema(connection)
    }

    /// Bring the runtime's projections up to date as the store opens: a store in shared memory
    /// is new, and a store on disk may need a replay after an upgrade.
    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()>;

    /// The digest of the claim kinds and fields this build knows. Peers whose digests differ may
    /// project the same claims differently, so they compare their logs instead of their graphs.
    fn schema_digest(&self) -> String;

    /// Claim vocabulary and shared layout before per-table definition versions. An older
    /// peer advertises this identity as schema_digest. No admission depends on this value.
    fn projection_registry_digest(&self) -> String {
        self.schema_digest()
    }

    /// Definition versions different from the implicit legacy version 1. All table digests
    /// stay advertised; a comparison skips only tables with different definition versions.
    fn projection_versions(&self) -> BTreeMap<String, u64> {
        BTreeMap::new()
    }

    /// Check a replicated claim's kind and fields once the graph has verified its hash and batch.
    fn classify_replicated_claim(
        &self,
        connection: &Connection,
        batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, Error>;

    /// `classify_replicated_claim` for a runtime whose answer depends only on the claim and this
    /// build, never on the store: admission then classifies before it takes the writer. `None`,
    /// the default, means the runtime reads the store, and the claim is classified under the
    /// writer as before.
    fn classify_replicated_claim_unbound(
        &self,
        _batch: &ReplicaBatch,
        _claim: &ClaimRecord,
    ) -> Option<Result<ReplicatedClaimAdmission, Error>> {
        None
    }

    /// Append a claim this node writes, from a client's input. The runtime decides how the claim
    /// is kept (some kinds stay local observations), validates it, appends it through the graph
    /// and projects it. The bool says whether the claim is new rather than an idempotent repeat.
    fn append_claim(&self, store: &Store, input: &ClaimInput)
    -> Result<(ClaimRecord, bool), Error>;

    /// Apply a replicated repair: `replacement` now stands for `repaired` in the runtime's
    /// projections.
    fn apply_repair_tx(
        &self,
        transaction: &Transaction<'_>,
        repaired: &str,
        replacement: &str,
    ) -> Result<()>;

    /// Append a claim this node writes inside the store's transaction: the runtime validates it,
    /// the graph assigns its batch and ID, and the runtime projects it.
    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<ClaimRecord>;

    /// Project newly admitted replicated claims from a healthy frontier through `through`.
    /// The store commits that frontier before lending the writer to the next chunk. Aggregates
    /// may rebuild from their complete history. `Replay(reason)` asks for a replay from nothing.
    fn project_incremental(
        &self,
        transaction: &Transaction<'_>,
        origin: &str,
        through: u64,
    ) -> Result<IncrementalProjection, Error>;

    /// Clear every shared projection and fold the claims into it again in canonical order.
    fn replay_from_nothing(&self, transaction: &Transaction<'_>) -> Result<(), Error>;

    /// Publish bounded stage progress without reading the database from the observer.
    fn replay_from_nothing_with_progress(
        &self,
        transaction: &Transaction<'_>,
        progress: &mut dyn FnMut(ReplayProgress),
    ) -> Result<(), Error> {
        progress(ReplayProgress {
            phase: "full-replay",
            processed: None,
            total: None,
        });
        self.replay_from_nothing(transaction)
    }

    /// Reapply this node's local state that a projection replaced, once a projection ends.
    fn after_projection(&self, transaction: &Transaction<'_>) -> Result<(), Error>;

    /// Projections changed beneath whatever the runtime keeps in memory about them.
    fn forget_views(&self);

    /// Shared tables the projection digest covers, with the columns each leaves out.
    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])];

    /// The tables of the legacy six-table graph digest.
    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable];

    /// The digest of the rules that decide what a checkpoint drops. Nodes agree on a checkpoint
    /// only when their rules digests match.
    fn checkpoint_rules_digest(&self) -> String;

    /// Fail before sealing history or copying a store when a runtime has no checkpoint contract.
    fn checkpoint_preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Decide what a checkpoint drops from `sealed`: a pure function of the sealed claims.
    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan;

    /// What the staged planner reads of one sealed claim's body, computed while the capture
    /// holds the body. A runtime with no checkpoint rules reads nothing.
    fn checkpoint_claim_facts(&self, _claim: &ClaimRecord) -> CheckpointClaimFacts {
        CheckpointClaimFacts::default()
    }

    /// Decide what a checkpoint drops from a staged set, by marking its staged claims: the same
    /// decision `plan_checkpoint_drops` makes from the set in memory. Marking nothing drops
    /// nothing.
    fn plan_staged_checkpoint_drops(&self, _planner: &StagedPlanner<'_>) -> Result<()> {
        Ok(())
    }

    /// Empty every projection, and every local table that refers to claims, on a copy of the
    /// store that a checkpoint proof is about to cut down to its sealed set.
    fn clear_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()>;

    /// Project a checkpoint proof's copy from nothing.
    fn replay_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()>;

    /// Every answer about `subject` that a checkpoint must leave unchanged, as of the cut.
    fn checkpoint_subject_answers(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<Value>;

    /// Answers that belong to no one subject, such as a fold across every seat, that a checkpoint
    /// must leave unchanged. The proof computes them on its copy, which they may write to, before
    /// and after the drop, and compares them with the subjects' answers.
    fn checkpoint_global_answers(&self, _transaction: &Transaction<'_>) -> Result<Option<Value>> {
        Ok(None)
    }

    /// Return the same proof answers, with optional bounded per-item source identities captured
    /// during that read. Sources are diagnostic only and never enter the proof/digest contract.
    fn checkpoint_subject_answers_with_sources(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<(Value, super::checkpoint::CheckpointAnswerSources)> {
        Ok((
            self.checkpoint_subject_answers(connection, subject, cut)?,
            Default::default(),
        ))
    }
}

/// The runtime of a program that only keeps and syncs claims: it accepts every claim as it is,
/// keeps no tables of its own, and projects nothing. It has no checkpoint rules, so a fleet of
/// plain stores never seals a checkpoint.
#[derive(Clone, Copy, Debug, Default)]
pub struct Plain;

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

    fn append_claim(
        &self,
        store: &Store,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), Error> {
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
                .map_err(crate::error::typed)
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
        _through: u64,
    ) -> Result<IncrementalProjection, Error> {
        Ok(IncrementalProjection::Projected)
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
