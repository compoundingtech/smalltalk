//! What a runtime built on the graph plugs into the store.
//!
//! The graph knows claims, envelopes, batches, documents and blobs, but not what any claim kind
//! means. A runtime knows its kinds: their schemas, the tables it projects them into, and which of
//! them a checkpoint may drop. The store calls it at each of these seams, inside the store's own
//! transactions, and never the other way around: the runtime may call the graph freely.

use anyhow::Result;
use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};

use super::checkpoint::{DropPlan, SealedSet};
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

    /// Bring the runtime's projections up to date as the store opens: a store in shared memory
    /// is new, and a store on disk may need a replay after an upgrade.
    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()>;

    /// The digest of the claim kinds and fields this build knows. Peers whose digests differ may
    /// project the same claims differently, so they compare their logs instead of their graphs.
    fn schema_digest(&self) -> String;

    /// Current observations may live outside immutable claim authority. Let the runtime
    /// select their read sources while the graph retains its existing fold and timestamps.
    /// Immutable runtimes use the original indexed query unchanged.
    fn current_observation_sql(&self, _kind: &str, sql: &str) -> String {
        sql.to_owned()
    }

    /// Process-local invalidation for replaceable observations committed outside the graph
    /// writer. This is a cache hint, never an immutable graph or authority frontier.
    fn current_observation_revision(&self) -> u64 {
        0
    }

    /// Called only after a current-observation transaction successfully commits. The hook
    /// must be non-blocking and do no SQL; immutable runtimes need no invalidation.
    fn current_observation_committed(&self) {}

    /// Check a replicated claim's kind and fields once the graph has verified its hash and batch.
    fn classify_replicated_claim(
        &self,
        connection: &Connection,
        batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, Error>;

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

    /// Return the same proof answers, with optional bounded per-item source identities captured
    /// during that read. Sources are diagnostic only and never enter the proof/digest contract.
    fn checkpoint_subject_answers_with_sources(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<(Value, super::checkpoint::CheckpointAnswerSources)> {
        Ok((self.checkpoint_subject_answers(connection, subject, cut)?, Default::default()))
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
