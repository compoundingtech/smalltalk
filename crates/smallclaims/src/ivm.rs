//! Transactional, keyed views over admitted claims.
//!
//! A view maps one admitted claim to a bounded set of register contributions. Register heads
//! are selected by a stable order supplied by the view (normally [`canonical::sortable_key`]).
//! Arrival order and duplicate delivery do not affect the answer. Retraction uses the claim
//! index and the next register candidate; it never reads the claim history.
//!
//! Integrations must call [`Views::change`] inside the admission/projection transaction, and
//! publish the matching [`SourceCut`] in that transaction. Unknown or rejected claims are not
//! inputs. Authority closures, ordered joins, deadlines and historical roots need their own
//! operators; a latest-value register does not implement those semantics.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ClaimRecord, store::canonical};

pub mod after_write;
pub mod claim_source;
pub mod events;
pub mod install;
pub mod runtime;

/// Changes to local provenance/status bookkeeping require an explicit fenced installation.
pub const LAYOUT: &str = "smallclaims.ivm.provenance-availability.v3";

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_source (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), epoch INTEGER NOT NULL,
    admitted INTEGER NOT NULL, projected INTEGER NOT NULL, local_generation INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ivm_views (
    name TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL,
    ready INTEGER NOT NULL, generation INTEGER NOT NULL,
    applied_claim_index INTEGER NOT NULL DEFAULT 0,
    applied_local_generation INTEGER NOT NULL DEFAULT 0,
    deferred_claim_index INTEGER NOT NULL DEFAULT 0,
    deferred_local_generation INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS ivm_view_errors (
    view TEXT PRIMARY KEY, error TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS ivm_contributions (
    view TEXT NOT NULL, key TEXT NOT NULL, register TEXT NOT NULL, claim_id TEXT NOT NULL,
    value TEXT NOT NULL, rank BLOB NOT NULL,
    PRIMARY KEY(view,key,register,claim_id)
);
CREATE INDEX IF NOT EXISTS ivm_contributions_claim ON ivm_contributions(view,claim_id,key,register);
CREATE INDEX IF NOT EXISTS ivm_contributions_rank ON ivm_contributions(view,key,register,rank DESC,claim_id DESC);
CREATE TABLE IF NOT EXISTS ivm_claim_ranks (
    claim_id TEXT PRIMARY KEY, position INTEGER NOT NULL
);
-- Provenance is independent of register contributions, including custom-only operators.
-- input_claim_id owns the dependency, so removing one input cannot erase another's reference.
CREATE TABLE IF NOT EXISTS ivm_claim_views (
    view TEXT NOT NULL, input_claim_id TEXT NOT NULL, claim_id TEXT NOT NULL,
    PRIMARY KEY(view,input_claim_id,claim_id)
);
CREATE INDEX IF NOT EXISTS ivm_claim_views_dependency ON ivm_claim_views(claim_id,view);
CREATE TABLE IF NOT EXISTS ivm_heads (
    view TEXT NOT NULL, key TEXT NOT NULL, register TEXT NOT NULL,
    value TEXT NOT NULL, claim_id TEXT NOT NULL, rank BLOB NOT NULL,
    PRIMARY KEY(view,key,register)
);
CREATE TABLE IF NOT EXISTS ivm_key_frontier (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), sequence INTEGER NOT NULL
);
INSERT OR IGNORE INTO ivm_key_frontier VALUES(1,0);
CREATE TABLE IF NOT EXISTS ivm_keys (
    view TEXT NOT NULL, key TEXT NOT NULL, generation INTEGER NOT NULL,
    changed_sequence INTEGER NOT NULL,
    PRIMARY KEY(view,key)
);
CREATE INDEX IF NOT EXISTS ivm_keys_changes ON ivm_keys(view,changed_sequence,key);
-- Current-state availability invalidations, separate from semantic key generations.
-- The source row is shared: unrelated admission never writes every registered view.
CREATE TABLE IF NOT EXISTS ivm_status_frontier (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1), sequence INTEGER NOT NULL,
    source_sequence INTEGER NOT NULL, pending INTEGER NOT NULL
);
INSERT OR IGNORE INTO ivm_status_frontier VALUES(1,0,0,1);
CREATE TABLE IF NOT EXISTS ivm_view_status (
    view TEXT PRIMARY KEY, sequence INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS ivm_view_status_insert AFTER INSERT ON ivm_views
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(NEW.name,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
CREATE TRIGGER IF NOT EXISTS ivm_view_status_update
AFTER UPDATE OF ready,fingerprint,epoch ON ivm_views
WHEN OLD.ready<>NEW.ready OR OLD.fingerprint<>NEW.fingerprint OR OLD.epoch<>NEW.epoch
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(NEW.name,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
CREATE TRIGGER IF NOT EXISTS ivm_view_status_delete AFTER DELETE ON ivm_views
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(OLD.name,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
-- Error evidence is part of availability even when ready remains false. Identical evidence
-- must not advance a cursor; removal is observable as a cleared current error.
CREATE TRIGGER IF NOT EXISTS ivm_error_status_insert AFTER INSERT ON ivm_view_errors
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(NEW.view,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
CREATE TRIGGER IF NOT EXISTS ivm_error_status_update AFTER UPDATE OF error ON ivm_view_errors
WHEN OLD.error<>NEW.error
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(NEW.view,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
CREATE TRIGGER IF NOT EXISTS ivm_error_status_delete AFTER DELETE ON ivm_view_errors
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1 WHERE singleton=1;
    INSERT INTO ivm_view_status VALUES(OLD.view,(SELECT sequence FROM ivm_status_frontier))
        ON CONFLICT(view) DO UPDATE SET sequence=excluded.sequence;
END;
CREATE TRIGGER IF NOT EXISTS ivm_source_pending_admission AFTER INSERT ON claims
WHEN NEW.store_index>(SELECT projected FROM ivm_source WHERE singleton=1)
    AND (SELECT pending FROM ivm_status_frontier WHERE singleton=1)=0
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1,source_sequence=sequence+1,pending=1
        WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS ivm_source_status_insert AFTER INSERT ON ivm_source
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1,source_sequence=sequence+1,
        pending=(NEW.admitted<>NEW.projected OR NEW.admitted<>MAX(
            COALESCE((SELECT MAX(store_index) FROM claims),0),
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name='claims'),0)))
        WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS ivm_source_status_update
AFTER UPDATE OF admitted,projected,epoch ON ivm_source
WHEN OLD.epoch<>NEW.epoch OR (SELECT pending FROM ivm_status_frontier WHERE singleton=1)<>
    (NEW.admitted<>NEW.projected OR NEW.admitted<>MAX(
        COALESCE((SELECT MAX(store_index) FROM claims),0),
        COALESCE((SELECT seq FROM sqlite_sequence WHERE name='claims'),0)))
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1,source_sequence=sequence+1,
        pending=(NEW.admitted<>NEW.projected OR NEW.admitted<>MAX(
            COALESCE((SELECT MAX(store_index) FROM claims),0),
            COALESCE((SELECT seq FROM sqlite_sequence WHERE name='claims'),0)))
        WHERE singleton=1;
END;
CREATE TRIGGER IF NOT EXISTS ivm_source_status_delete AFTER DELETE ON ivm_source
BEGIN
    UPDATE ivm_status_frontier SET sequence=sequence+1,source_sequence=sequence+1,pending=1
        WHERE singleton=1;
END;
-- Canonical rank changes without a frontier advance cannot silently serve existing roots.
-- Fencing is bounded by views already depending on the changed claim, never claim history.
-- Indexed EXISTS probes each finite registered view; do not enumerate all referencing inputs
-- when many custom facts depend on the same claim.
-- Replace the older prototype's contribution-only triggers; its fingerprint is incompatible.
DROP TRIGGER IF EXISTS ivm_record_rank_insert;
DROP TRIGGER IF EXISTS ivm_record_rank_delete;
DROP TRIGGER IF EXISTS ivm_record_rank_update;
DROP TRIGGER IF EXISTS ivm_claim_source_update;
CREATE TRIGGER IF NOT EXISTS ivm_record_rank_insert AFTER INSERT ON replica_records
WHEN NEW.claim_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM ivm_claim_ranks WHERE claim_id=NEW.claim_id AND position=(
        SELECT MIN(position) FROM replica_records WHERE claim_id=NEW.claim_id))
BEGIN
    UPDATE ivm_views SET ready=0 WHERE ready<>0 AND EXISTS
        (SELECT 1 FROM ivm_claim_views WHERE claim_id=NEW.claim_id AND view=ivm_views.name);
END;
CREATE TRIGGER IF NOT EXISTS ivm_record_rank_delete AFTER DELETE ON replica_records
WHEN OLD.claim_id IS NOT NULL
BEGIN
    UPDATE ivm_views SET ready=0 WHERE ready<>0 AND EXISTS
        (SELECT 1 FROM ivm_claim_views WHERE claim_id=OLD.claim_id AND view=ivm_views.name);
END;
CREATE TRIGGER IF NOT EXISTS ivm_record_rank_update AFTER UPDATE OF position,claim_id ON replica_records
WHEN OLD.position<>NEW.position OR OLD.claim_id IS NOT NEW.claim_id
BEGIN
    UPDATE ivm_views SET ready=0 WHERE ready<>0 AND EXISTS
        (SELECT 1 FROM ivm_claim_views WHERE claim_id IN (OLD.claim_id,NEW.claim_id) AND view=ivm_views.name);
END;
-- Legacy COUNT ranks may change for surviving claims when an earlier row disappears or is
-- renumbered. v0 fences the finite registry, without enumerating a growing batch. A reviewed
-- rank dependency operator is required to resume these views; no replay fallback is provided.
CREATE TRIGGER IF NOT EXISTS ivm_claim_source_delete AFTER DELETE ON claims
BEGIN UPDATE ivm_views SET ready=0; END;
CREATE TRIGGER IF NOT EXISTS ivm_claim_source_update
AFTER UPDATE OF store_index,batch_id,accepted_at_unix_ms,id,kind,subject,actor,body,predecessors ON claims
WHEN OLD.store_index<>NEW.store_index OR OLD.batch_id<>NEW.batch_id
    OR OLD.accepted_at_unix_ms<>NEW.accepted_at_unix_ms OR OLD.id<>NEW.id
    OR OLD.kind<>NEW.kind OR OLD.subject<>NEW.subject OR OLD.actor IS NOT NEW.actor
    OR OLD.body<>NEW.body OR OLD.predecessors<>NEW.predecessors
BEGIN UPDATE ivm_views SET ready=0; END;
CREATE TRIGGER IF NOT EXISTS ivm_batch_source_update
AFTER UPDATE OF origin,replica_sequence,id ON batches
WHEN OLD.origin<>NEW.origin OR OLD.replica_sequence<>NEW.replica_sequence OR OLD.id<>NEW.id
BEGIN UPDATE ivm_views SET ready=0; END;
"#;

/// A source identity, not a winner order. Epoch changes fence renumbered/reinterpreted sources.
/// Local observations have a separate generation; deadlines require a separate watermark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCut {
    pub epoch: u64,
    pub admitted: u64,
    pub projected: u64,
    pub local_generation: u64,
}

impl SourceCut {
    fn validate(self) -> Result<()> {
        ensure!(
            self.projected <= self.admitted,
            "projected frontier exceeds admission"
        );
        for value in [
            self.epoch,
            self.admitted,
            self.projected,
            self.local_generation,
        ] {
            ensure!(
                value <= i64::MAX as u64,
                "IVM cut exceeds SQLite integer range"
            );
        }
        Ok(())
    }
}

/// Include schema, claim registry, authority rules and dependency extractor versions here.
/// Changing any input semantics requires a different fingerprint and explicit initialization.
#[derive(Clone, Copy, Debug)]
pub struct Definition {
    pub name: &'static str,
    pub fingerprint: &'static str,
    pub kinds: &'static [&'static str],
    /// Authenticated current observations/deadline adapters, separate from durable claims.
    pub local_kinds: &'static [&'static str],
    /// Bound on this view's work per input; excess fences the projection, preserving admission.
    pub max_contributions: usize,
}

/// One claim's contribution to one register. `rank` must be stable across nodes and arrivals.
/// Removal is represented by an explicit value/tombstone, never omission of an older head.
#[derive(Clone, Debug, PartialEq)]
pub struct Contribution {
    pub key: String,
    pub register: String,
    pub value: Value,
    pub rank: Vec<u8>,
}

/// Repair eligibility is a per-kind projection rule, not a replica record-state predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairPolicy {
    /// Keep the admitted original (for example message.sent), also admitting the replacement.
    RetainOriginal,
    /// Remove the original contribution when the runtime accepts its replacement.
    ReplaceOriginal,
}

/// A bounded one-claim mapping. Implementations must not scan history or a complete roster.
/// Contributions store old dependency keys, so retraction does not lose an old actor/assignee.
/// Repeated (key,register) writes in one atomic claim use the last write, as operations do.
pub trait View: Send + Sync {
    fn definition(&self) -> Definition;
    fn repair_policy(&self) -> RepairPolicy {
        RepairPolicy::RetainOriginal
    }
    fn contributions(
        &self,
        _claim: &ClaimRecord,
        _canonical: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        Ok(Vec::new())
    }

    /// Optional indexed view-owned tables. Production runtimes include shared tables in their
    /// existing digest/checkpoint rules; the claims-only adapter has no custom retention proof.
    fn create_schema(&self, _connection: &Connection) -> Result<()> {
        Ok(())
    }

    /// Extra old/new reverse dependencies, found by indexed keyed lookups. This is called only
    /// for declared kinds. The contribution bound also bounds this affected-key set.
    fn affected_keys(
        &self,
        _transaction: &Transaction<'_>,
        _old: Option<&ClaimRecord>,
        _new: Option<&ClaimRecord>,
    ) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
    }

    /// Additional canonical claim dependencies used by custom joins. The input claim itself
    /// is always registered, even with zero contributions/keys. References must be complete
    /// and bounded; untracked external SQL state needs its own reviewed invalidation hook.
    fn canonical_dependencies(&self, _claim: &ClaimRecord) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
    }

    /// Maintain one indexed custom output in the same transaction as the registers and cut.
    /// Return Some(changed) to define semantic equality for the actual public output; None
    /// uses register-head equality. Changes must include content, order, membership and auth.
    /// Implementations must prove duplicate/retraction/order independence and the key bound;
    /// the register primitive cannot prove an arbitrary authority closure or ordered join.
    fn maintain_local_key(
        &self,
        transaction: &Transaction<'_>,
        key: &str,
        _change: &LocalChange,
    ) -> Result<bool> {
        self.maintain_key(transaction, key, None, None)?
            .context("view has no current-source operator")
    }

    fn maintain_key(
        &self,
        _transaction: &Transaction<'_>,
        _key: &str,
        _old: Option<&ClaimRecord>,
        _new: Option<&ClaimRecord>,
    ) -> Result<Option<bool>> {
        Ok(None)
    }
}

/// Affected-but-identical keys are separate from keys whose visible register heads changed.
/// Heads include their revision and rank: visible order/authorization tokens are semantic.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Changes {
    pub affected: BTreeSet<(String, String)>,
    pub changed: BTreeSet<(String, String)>,
    /// Admitted source is preserved while these projections fence reads/effects.
    pub deferred: BTreeSet<String>,
}

/// A normalized admitted current-source change. The adapter writes its authenticated latest
/// source row in this transaction; no durable telemetry claim is created. Old/new owner keys
/// and sample/window/reset/freshness interpretation belong to the versioned source adapter.
#[derive(Clone, Debug)]
pub struct LocalChange {
    pub kind: String,
    pub old_keys: BTreeSet<String>,
    pub new_keys: BTreeSet<String>,
    pub evaluation_time_unix_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub fingerprint: String,
    pub epoch: u64,
    pub generation: u64,
}

/// Latest changed-key invalidations, coalesced by key. Deleted outputs retain a key token so
/// removals cannot disappear from the feed. This is not a history of intermediate answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready(Token),
    Missing,
    Fenced,
    VersionMismatch,
    EpochMismatch,
    SourcePending,
}

/// Restart-safe, coalesced availability cursor, readable even when output is fenced.
/// This does not witness every intermediate transition or substitute for an action stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailabilityToken {
    pub fingerprint: String,
    pub epoch: u64,
    pub source_sequence: u64,
    pub view_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Availability {
    pub token: AvailabilityToken,
    pub frontier: u64,
    pub readiness: Readiness,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionState {
    pub readiness: Readiness,
    pub applied_claim_index: Option<u64>,
    pub applied_local_generation: Option<u64>,
    pub deferred_claim_index: Option<u64>,
    pub deferred_local_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChange {
    pub key: String,
    pub generation: u64,
    pub sequence: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPage {
    pub token: Token,
    pub keys: Vec<KeyChange>,
    pub frontier: u64,
    pub next_after: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Head {
    pub value: Value,
    pub claim_id: String,
    pub rank: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegisterEvidence {
    pub readiness: Readiness,
    pub expected: Option<Head>,
    pub actual: Option<Head>,
    /// None is incomplete evidence, never a successful invariant result.
    pub matches: Option<bool>,
}

type StoredHead = (String, String, Vec<u8>);

/// Construct once at runtime initialization. Exact kinds dispatch before any SQLite work.
/// `custom.*.*` explicitly opts into two-component custom kinds; it is part of the fingerprint.
pub struct Views {
    views: Vec<Box<dyn View>>,
    exact: BTreeMap<&'static str, Vec<usize>>,
    custom: Vec<usize>,
    local: BTreeMap<&'static str, Vec<usize>>,
    fingerprints: Vec<String>,
}

impl Views {
    pub fn new(views: Vec<Box<dyn View>>) -> Result<Self> {
        let mut names = BTreeSet::new();
        let mut fingerprints = Vec::new();
        let mut exact = BTreeMap::<_, Vec<_>>::new();
        let mut custom = Vec::new();
        let mut local = BTreeMap::<_, Vec<_>>::new();
        for (index, view) in views.iter().enumerate() {
            let definition = view.definition();
            use sha2::{Digest, Sha256};
            fingerprints.push(hex::encode(Sha256::digest(serde_json::to_vec(&(
                LAYOUT,
                definition.name,
                definition.fingerprint,
                definition.kinds,
                definition.local_kinds,
                definition.max_contributions,
                format!("{:?}", view.repair_policy()),
            ))?)));
            ensure!(
                !definition.name.is_empty() && !definition.fingerprint.is_empty(),
                "empty view identity"
            );
            ensure!(names.insert(definition.name), "duplicate view name");
            ensure!(definition.max_contributions > 0, "zero contribution bound");
            let mut local_kinds = BTreeSet::new();
            for &kind in definition.local_kinds {
                ensure!(
                    !kind.is_empty() && !kind.contains('*') && local_kinds.insert(kind),
                    "invalid/duplicate current-source kind"
                );
                local.entry(kind).or_default().push(index);
            }
            let mut kinds = BTreeSet::new();
            for &kind in definition.kinds {
                ensure!(kinds.insert(kind), "duplicate input kind");
                if kind == "custom.*.*" {
                    custom.push(index);
                } else {
                    ensure!(
                        !kind.is_empty() && !kind.contains('*'),
                        "unsupported input pattern"
                    );
                    exact.entry(kind).or_default().push(index);
                }
            }
        }
        Ok(Self {
            views,
            exact,
            custom,
            local,
            fingerprints,
        })
    }

    fn subscribers(&self, kind: &str) -> BTreeSet<usize> {
        let mut result = self
            .exact
            .get(kind)
            .into_iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        if let Some(tail) = kind.strip_prefix("custom.")
            && let Some((namespace, name)) = tail.split_once('.')
            && !namespace.is_empty()
            && !name.is_empty()
            && !name.contains('.')
        {
            result.extend(self.custom.iter().copied());
        }
        result
    }

    /// Lets the runtime skip canonical-key capture for an unrelated kind.
    pub fn reads_kind(&self, kind: &str) -> bool {
        !self.subscribers(kind).is_empty()
    }

    pub fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        for view in &self.views {
            view.create_schema(connection)?;
        }
        Ok(())
    }

    /// Publish source progress once per writer transaction, including transactions with no
    /// relevant kinds. Per-view rows remain untouched by those unrelated transactions.
    /// A new epoch requires explicit initialization/fencing; it cannot silently relabel roots.
    pub fn publish_cut(&self, transaction: &Transaction<'_>, cut: SourceCut) -> Result<()> {
        cut.validate()?;
        if let Some(previous) = source_cut(transaction)? {
            ensure!(
                cut.epoch == previous.epoch,
                "IVM source epoch requires explicit transition"
            );
            ensure!(
                cut.admitted >= previous.admitted
                    && cut.projected >= previous.projected
                    && cut.local_generation >= previous.local_generation,
                "IVM frontier moved backwards"
            );
        }
        transaction.execute(
            "INSERT INTO ivm_source VALUES(1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET
             admitted=excluded.admitted,projected=excluded.projected,local_generation=excluded.local_generation",
            params![cut.epoch,cut.admitted,cut.projected,cut.local_generation],
        )?;
        Ok(())
    }

    /// Start an explicit bounded initializer. This fences reads without scanning/deleting
    /// history or existing roots. The caller must populate a fresh epoch outside read/startup
    /// critical paths; the v0 primitive does not perform a backfill or promise historical roots.
    /// Currently initialization is supported only for empty view state, or the same ready
    /// fingerprint and epoch on reopen. An incompatible existing view stays unready.
    pub fn initialize_empty(&self, transaction: &Transaction<'_>, cut: SourceCut) -> Result<()> {
        cut.validate()?;
        self.publish_cut(transaction, cut)?;
        for (index, view) in self.views.iter().enumerate() {
            let d = view.definition();
            let expected = &self.fingerprints[index];
            let existing = transaction
                .query_row(
                    "SELECT fingerprint,epoch FROM ivm_views WHERE name=?1",
                    [d.name],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
                )
                .optional()?;
            if let Some((fingerprint, epoch)) = existing {
                if &fingerprint != expected || epoch != cut.epoch {
                    transaction.execute("UPDATE ivm_views SET ready=0 WHERE name=?1", [d.name])?;
                }
                continue;
            }
            let populated: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM ivm_contributions WHERE view=?1)",
                [d.name],
                |row| row.get(0),
            )?;
            ensure!(
                !populated,
                "missing view identity with existing contributions"
            );
            transaction.execute(
                "INSERT INTO ivm_views(name,fingerprint,epoch,ready,generation) VALUES(?1,?2,?3,?4,0)",
                params![d.name, expected, cut.epoch, cut.admitted == 0],
            )?;
        }
        Ok(())
    }

    /// A changed admitted input, including canonical reordering with no frontier advance.
    /// `old` identifies the contribution to retract; `new` supplies its replacement. The caller
    /// must supply the canonical tuple captured at admission, not derive legacy position by
    /// scanning a growing batch. Empty/unrelated changes execute zero SQL.
    pub fn change(
        &self,
        transaction: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
        epoch: u64,
    ) -> Result<Changes> {
        self.change_selected(transaction, old, new, epoch, None)
    }

    /// Apply the runtime's accepted repair using each view's explicit eligibility policy.
    /// Record state alone does not retract admitted facts; canonical position ignores state.
    pub fn repair(
        &self,
        transaction: &Transaction<'_>,
        old: &ClaimRecord,
        replacement: (&ClaimRecord, &canonical::ClaimKey),
        epoch: u64,
    ) -> Result<Changes> {
        let mut subscribers = self.subscribers(&old.kind);
        subscribers.extend(self.subscribers(&replacement.0.kind));
        let replace = subscribers
            .iter()
            .copied()
            .filter(|&index| self.views[index].repair_policy() == RepairPolicy::ReplaceOriginal)
            .collect::<BTreeSet<_>>();
        let retain = subscribers
            .difference(&replace)
            .copied()
            .collect::<BTreeSet<_>>();
        let mut changes = self.change_selected(
            transaction,
            Some(old),
            Some(replacement),
            epoch,
            Some(&replace),
        )?;
        let admitted =
            self.change_selected(transaction, None, Some(replacement), epoch, Some(&retain))?;
        changes.affected.extend(admitted.affected);
        changes.changed.extend(admitted.changed);
        changes.deferred.extend(admitted.deferred);
        Ok(changes)
    }

    fn change_selected(
        &self,
        transaction: &Transaction<'_>,
        old: Option<&ClaimRecord>,
        new: Option<(&ClaimRecord, &canonical::ClaimKey)>,
        epoch: u64,
        selected: Option<&BTreeSet<usize>>,
    ) -> Result<Changes> {
        let mut subscribers = BTreeSet::new();
        if let Some(old) = old {
            subscribers.extend(self.subscribers(&old.kind));
        }
        if let Some((new, _)) = new {
            subscribers.extend(self.subscribers(&new.kind));
        }
        let mut changes = Changes::default();
        for index in subscribers {
            if selected.is_some_and(|selected| !selected.contains(&index)) {
                continue;
            }
            let view = &self.views[index];
            let d = view.definition();
            if !matches!(
                self.view_readiness(transaction, d.name, epoch)?,
                Readiness::Ready(_)
            ) {
                transaction.execute("UPDATE ivm_views SET deferred_claim_index=MAX(deferred_claim_index,?2) WHERE name=?1",params![d.name,new.map(|(c,_)|c.store_index).unwrap_or(0)])?;
                changes.deferred.insert(d.name.to_owned());
                continue;
            }
            transaction.execute_batch("SAVEPOINT ivm_view_change")?;
            let mut view_changes = Changes::default();
            let result = (|| -> Result<()> {
                let extra_keys =
                    view.affected_keys(transaction, old, new.map(|(record, _)| record))?;
                ensure!(
                    extra_keys.len() <= d.max_contributions,
                    "affected-key bound exceeded"
                );
                let mut affected_keys = extra_keys;
                let mut before = BTreeMap::<(String, String), Option<StoredHead>>::new();
                if let Some(old) = old {
                    transaction.execute(
                        "DELETE FROM ivm_claim_views WHERE view=?1 AND input_claim_id=?2",
                        params![d.name, old.id],
                    )?;
                    let mut statement = transaction.prepare_cached(
                    "SELECT key,register FROM ivm_contributions WHERE view=?1 AND claim_id=?2 ORDER BY key,register",
                )?;
                    let keys = statement
                        .query_map(params![d.name, old.id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    ensure!(
                        keys.len() <= d.max_contributions,
                        "stored contribution bound exceeded"
                    );
                    for key in keys {
                        before.insert(
                            key.clone(),
                            stored_head(transaction, d.name, &key.0, &key.1)?,
                        );
                    }
                    transaction.execute(
                        "DELETE FROM ivm_contributions WHERE view=?1 AND claim_id=?2",
                        params![d.name, old.id],
                    )?;
                }
                if let Some((claim, canonical)) = new
                    && self.subscribers(&claim.kind).contains(&index)
                {
                    transaction.execute(
                    "INSERT INTO ivm_claim_ranks VALUES(?1,?2) ON CONFLICT(claim_id) DO UPDATE SET position=excluded.position WHERE position<>excluded.position",
                    params![claim.id,canonical.4],
                )?;
                    let contributions = view.contributions(claim, canonical)?;
                    ensure!(
                        contributions.len() <= d.max_contributions,
                        "view contribution bound exceeded"
                    );
                    let mut dependencies = view.canonical_dependencies(claim)?;
                    ensure!(
                        dependencies.len() <= d.max_contributions,
                        "canonical dependency bound exceeded"
                    );
                    dependencies.insert(claim.id.clone());
                    for dependency in dependencies {
                        ensure!(!dependency.is_empty(), "empty canonical dependency");
                        transaction.execute(
                            "INSERT OR IGNORE INTO ivm_claim_views VALUES(?1,?2,?3)",
                            params![d.name, claim.id, dependency],
                        )?;
                    }
                    let mut writes = BTreeMap::new();
                    for contribution in contributions {
                        ensure!(
                            !contribution.key.is_empty() && !contribution.register.is_empty(),
                            "empty contribution key"
                        );
                        writes.insert(
                            (contribution.key.clone(), contribution.register.clone()),
                            contribution,
                        );
                    }
                    for (key, contribution) in writes {
                        if !before.contains_key(&key) {
                            before.insert(
                                key.clone(),
                                stored_head(transaction, d.name, &key.0, &key.1)?,
                            );
                        }
                        let value = serde_json::to_string(&contribution.value)?;
                        transaction.execute(
                        "INSERT INTO ivm_contributions VALUES(?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(view,key,register,claim_id) DO UPDATE SET value=excluded.value,rank=excluded.rank
                         WHERE value<>excluded.value OR rank<>excluded.rank",
                        params![d.name,contribution.key,contribution.register,claim.id,value,contribution.rank],
                    )?;
                    }
                }
                let mut changed_keys = BTreeSet::new();
                for ((key, register), old_head) in before {
                    affected_keys.insert(key.clone());
                    let next = transaction.query_row(
                    "SELECT value,claim_id,rank FROM ivm_contributions
                     WHERE view=?1 AND key=?2 AND register=?3 ORDER BY rank DESC,claim_id DESC LIMIT 1",
                    params![d.name,key,register], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Vec<u8>>(2)?)),
                ).optional()?;
                    if next == old_head {
                        continue;
                    }
                    match next {
                        Some((value, claim, rank)) => {
                            transaction.execute(
                            "INSERT INTO ivm_heads VALUES(?1,?2,?3,?4,?5,?6)
                             ON CONFLICT(view,key,register) DO UPDATE SET value=excluded.value,claim_id=excluded.claim_id,rank=excluded.rank",
                            params![d.name,key,register,value,claim,rank],
                        )?;
                        }
                        None => {
                            transaction.execute(
                                "DELETE FROM ivm_heads WHERE view=?1 AND key=?2 AND register=?3",
                                params![d.name, key, register],
                            )?;
                        }
                    }
                    changed_keys.insert(key);
                }
                ensure!(
                    affected_keys.len() <= d.max_contributions.saturating_mul(2),
                    "old/new affected-key union exceeds bound"
                );
                for key in &affected_keys {
                    view_changes
                        .affected
                        .insert((d.name.to_owned(), key.clone()));
                    if let Some(changed) =
                        view.maintain_key(transaction, key, old, new.map(|(record, _)| record))?
                    {
                        if changed {
                            changed_keys.insert(key.clone());
                        } else {
                            changed_keys.remove(key);
                        }
                    }
                }
                if !changed_keys.is_empty() {
                    transaction.execute(
                        "UPDATE ivm_views SET generation=generation+1 WHERE name=?1",
                        [d.name],
                    )?;
                    for key in changed_keys {
                        note_key_change(transaction, d.name, &key)?;
                        view_changes.changed.insert((d.name.to_owned(), key));
                    }
                }
                if let Some((claim, _)) = new {
                    transaction.execute("UPDATE ivm_views SET applied_claim_index=MAX(applied_claim_index,?2) WHERE name=?1",params![d.name,claim.store_index])?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    transaction.execute_batch("RELEASE ivm_view_change")?;
                    changes.affected.extend(view_changes.affected);
                    changes.changed.extend(view_changes.changed);
                }
                Err(error) => {
                    transaction
                        .execute_batch("ROLLBACK TO ivm_view_change; RELEASE ivm_view_change")?;
                    // Storage/transaction failures must reach the owning transaction. They
                    // are not operator evidence and cannot turn a transient failure into a
                    // persistent unavailable view while acknowledging the source write.
                    if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
                        return Err(error);
                    }
                    fence_error(transaction, d.name, &error)?;
                    transaction.execute("UPDATE ivm_views SET deferred_claim_index=MAX(deferred_claim_index,?2) WHERE name=?1",params![d.name,new.map(|(c,_)|c.store_index).unwrap_or(0)])?;
                    changes.deferred.insert(d.name.to_owned());
                }
            }
        }
        Ok(changes)
    }

    /// Apply an authenticated latest-source/deadline change without inventing a graph claim.
    /// The adapter owns proof, actual sample time, windows/reset/exhaustion and reverse keys.
    /// Source rows, public output, semantic generations and local source cut commit together.
    pub fn local_change(
        &self,
        transaction: &Transaction<'_>,
        change: &LocalChange,
        cut: SourceCut,
    ) -> Result<Changes> {
        cut.validate()?;
        let previous = source_cut(transaction)?.context("IVM source unready")?;
        ensure!(
            cut.epoch == previous.epoch
                && cut.admitted == previous.admitted
                && cut.projected == previous.projected
                && cut.local_generation > previous.local_generation,
            "current-source change requires a fresh local generation and unchanged claim cut"
        );
        let keys = change
            .old_keys
            .union(&change.new_keys)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changes = Changes::default();
        if let Some(subscribers) = self.local.get(change.kind.as_str()) {
            for &index in subscribers {
                let view = &self.views[index];
                let d = view.definition();
                if !matches!(
                    self.view_readiness(transaction, d.name, cut.epoch)?,
                    Readiness::Ready(_)
                ) {
                    transaction.execute("UPDATE ivm_views SET deferred_local_generation=MAX(deferred_local_generation,?2) WHERE name=?1",params![d.name,cut.local_generation])?;
                    changes.deferred.insert(d.name.to_owned());
                    continue;
                }
                transaction.execute_batch("SAVEPOINT ivm_local_view_change")?;
                let mut view_changes = Changes::default();
                let result = (|| -> Result<()> {
                    ensure!(
                        keys.len() <= d.max_contributions.saturating_mul(2),
                        "current-source affected-key bound exceeded"
                    );
                    let mut changed = BTreeSet::new();
                    for key in &keys {
                        view_changes
                            .affected
                            .insert((d.name.to_owned(), key.clone()));
                        if view.maintain_local_key(transaction, key, change)? {
                            changed.insert(key);
                        }
                    }
                    if !changed.is_empty() {
                        transaction.execute(
                            "UPDATE ivm_views SET generation=generation+1 WHERE name=?1",
                            [d.name],
                        )?;
                        for key in changed {
                            note_key_change(transaction, d.name, key)?;
                            view_changes
                                .changed
                                .insert((d.name.to_owned(), key.clone()));
                        }
                    }
                    transaction.execute("UPDATE ivm_views SET applied_local_generation=MAX(applied_local_generation,?2) WHERE name=?1",params![d.name,cut.local_generation])?;
                    Ok(())
                })();
                match result {
                    Ok(()) => {
                        transaction.execute_batch("RELEASE ivm_local_view_change")?;
                        changes.affected.extend(view_changes.affected);
                        changes.changed.extend(view_changes.changed);
                    }
                    Err(error) => {
                        transaction.execute_batch(
                            "ROLLBACK TO ivm_local_view_change; RELEASE ivm_local_view_change",
                        )?;
                        if error.chain().any(|cause| cause.is::<rusqlite::Error>()) {
                            return Err(error);
                        }
                        fence_error(transaction, d.name, &error)?;
                        transaction.execute("UPDATE ivm_views SET deferred_local_generation=MAX(deferred_local_generation,?2) WHERE name=?1",params![d.name,cut.local_generation])?;
                        changes.deferred.insert(d.name.to_owned());
                    }
                }
            }
        }
        self.publish_cut(transaction, cut)?;
        Ok(changes)
    }

    /// Read a bounded page of current changed keys inside one short read snapshot. A caller
    /// continuing a page supplies its captured token; a changed view returns an explicit gap.
    /// Subscribe-before-frontier wake registration and retention are the graph-watch seam.
    pub fn changed_keys(
        &self,
        connection: &Connection,
        name: &str,
        epoch: u64,
        after: u64,
        limit: usize,
        expected: Option<&Token>,
    ) -> Result<KeyPage> {
        ensure!(
            (1..=1024).contains(&limit),
            "changed-key page limit outside 1..=1024"
        );
        let token = self.token(connection, name, epoch)?;
        if let Some(expected) = expected {
            ensure!(*expected == token, "IVM changed-key page gap: view changed");
        }
        let frontier: u64 = connection.query_row(
            "SELECT sequence FROM ivm_key_frontier WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            after <= frontier,
            "IVM changed-key page gap: frontier replaced"
        );
        let mut statement=connection.prepare_cached(
            "SELECT key,generation,changed_sequence FROM ivm_keys WHERE view=?1 AND changed_sequence>?2 AND changed_sequence<=?3 ORDER BY changed_sequence,key LIMIT ?4",
        )?;
        let mut keys = statement
            .query_map(params![name, after, frontier, limit + 1], |row| {
                Ok(KeyChange {
                    key: row.get(0)?,
                    generation: row.get(1)?,
                    sequence: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let more = keys.len() > limit;
        keys.truncate(limit);
        let next_after = if more {
            keys.last().map(|key| key.sequence)
        } else {
            None
        };
        Ok(KeyPage {
            token,
            keys,
            frontier,
            next_after,
        })
    }

    /// Validate identity/readiness in the same short read snapshot as the heads. A token is
    /// never silently rebound to an epoch or incompatible extractor. This current-state
    /// token does not promise historical versions, a deadline watermark or a retained cursor.
    fn view_readiness(&self, connection: &Connection, name: &str, epoch: u64) -> Result<Readiness> {
        let index = self
            .views
            .iter()
            .position(|v| v.definition().name == name)
            .context("unknown IVM view")?;
        let expected = &self.fingerprints[index];
        let row = connection
            .query_row(
                "SELECT fingerprint,epoch,ready,generation FROM ivm_views WHERE name=?1",
                [name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((fingerprint, stored_epoch, ready, generation)) = row else {
            return Ok(Readiness::Missing);
        };
        if &fingerprint != expected {
            return Ok(Readiness::VersionMismatch);
        }
        if stored_epoch != epoch {
            return Ok(Readiness::EpochMismatch);
        }
        if !ready {
            return Ok(Readiness::Fenced);
        }
        Ok(Readiness::Ready(Token {
            fingerprint,
            epoch,
            generation,
        }))
    }

    /// Read readiness includes the actual graph snapshot frontier, not just the last
    /// published metadata row. Replicated admission may commit before projection catches up.
    pub fn readiness(&self, connection: &Connection, name: &str, epoch: u64) -> Result<Readiness> {
        let state = self.view_readiness(connection, name, epoch)?;
        if !matches!(state, Readiness::Ready(_)) {
            return Ok(state);
        }
        let Some(source) = source_cut(connection)? else {
            return Ok(Readiness::SourcePending);
        };
        if source.epoch != epoch
            || source.admitted != source.projected
            || source.admitted != crate::store::current_index(connection)?
        {
            return Ok(Readiness::SourcePending);
        }
        Ok(state)
    }

    /// Read status independently of a ready output token, inside one short read snapshot.
    /// Compare this cursor after subscribe-before-frontier registration and after each wake.
    /// Source pending/ready transitions use a shared counter; same-index fences use the view
    /// counter. Neither changes semantic key generations. Publication/wake transport is the
    /// graph-watch integration, and retention/restore must fence epochs instead of reusing it.
    pub fn availability(
        &self,
        connection: &Connection,
        name: &str,
        epoch: u64,
    ) -> Result<Availability> {
        let index = self
            .views
            .iter()
            .position(|v| v.definition().name == name)
            .context("unknown IVM view")?;
        let readiness = self.readiness(connection, name, epoch)?;
        let (frontier, source_sequence) = connection.query_row(
            "SELECT sequence,source_sequence FROM ivm_status_frontier WHERE singleton=1",
            [],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        let view_sequence = connection
            .query_row(
                "SELECT sequence FROM ivm_view_status WHERE view=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let error = connection
            .query_row(
                "SELECT error FROM ivm_view_errors WHERE view=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(Availability {
            token: AvailabilityToken {
                fingerprint: self.fingerprints[index].clone(),
                epoch,
                source_sequence,
                view_sequence,
            },
            frontier,
            readiness,
            error,
        })
    }

    /// Evidence of this projection's work, separately from the canonical source cut. A
    /// deferred view never advances its applied frontier or a ready token for missed input.
    pub fn projection_state(
        &self,
        connection: &Connection,
        name: &str,
        epoch: u64,
    ) -> Result<ProjectionState> {
        let readiness = self.readiness(connection, name, epoch)?;
        let row=connection.query_row(
            "SELECT applied_claim_index,applied_local_generation,deferred_claim_index,deferred_local_generation FROM ivm_views WHERE name=?1",[name],
            |row| Ok((row.get::<_,u64>(0)?,row.get::<_,u64>(1)?,row.get::<_,u64>(2)?,row.get::<_,u64>(3)?)),
        ).optional()?;
        Ok(ProjectionState {
            readiness,
            applied_claim_index: row.map(|r| r.0),
            applied_local_generation: row.map(|r| r.1),
            deferred_claim_index: row.map(|r| r.2),
            deferred_local_generation: row.map(|r| r.3),
        })
    }

    pub fn token(&self, connection: &Connection, name: &str, epoch: u64) -> Result<Token> {
        match self.readiness(connection, name, epoch)? {
            Readiness::Ready(token) => Ok(token),
            state => bail!("IVM view unready: {state:?}"),
        }
    }

    /// Compare the indexed expected register relation with the actual projection. Readiness
    /// means source processing is complete; it is not a global integrity proof. Missing or
    /// corrupt actual rows remain detectable without healing/replaying during this read.
    pub fn register_evidence(
        &self,
        connection: &Connection,
        name: &str,
        key: &str,
        register: &str,
        epoch: u64,
    ) -> Result<RegisterEvidence> {
        let readiness = self.readiness(connection, name, epoch)?;
        let expected=connection.query_row(
            "SELECT value,claim_id,rank FROM ivm_contributions WHERE view=?1 AND key=?2 AND register=?3 ORDER BY rank DESC,claim_id DESC LIMIT 1",
            params![name,key,register],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Vec<u8>>(2)?)),
        ).optional()?;
        let actual = stored_head(connection, name, key, register)?;
        let matches = if matches!(readiness, Readiness::Ready(_)) {
            Some(expected == actual)
        } else {
            None
        };
        let convert = |stored: Option<StoredHead>| -> Result<Option<Head>> {
            stored
                .map(|(value, claim_id, rank)| {
                    Ok(Head {
                        value: serde_json::from_str(&value)?,
                        claim_id,
                        rank,
                    })
                })
                .transpose()
        };
        Ok(RegisterEvidence {
            readiness,
            expected: convert(expected)?,
            actual: convert(actual)?,
            matches,
        })
    }

    pub fn head(
        &self,
        connection: &Connection,
        name: &str,
        key: &str,
        register: &str,
        cut: SourceCut,
    ) -> Result<Option<Head>> {
        cut.validate()?;
        self.token(connection, name, cut.epoch)?;
        let actual = source_cut(connection)?.context("IVM source unready")?;
        ensure!(
            actual == cut && cut.admitted == cut.projected,
            "IVM source stale or projection pending"
        );
        let evidence = self.register_evidence(connection, name, key, register, cut.epoch)?;
        ensure!(
            evidence.matches == Some(true),
            "IVM register invariant mismatch or incomplete evidence"
        );
        Ok(evidence.actual)
    }
}

fn stored_head(
    connection: &Connection,
    view: &str,
    key: &str,
    register: &str,
) -> Result<Option<StoredHead>> {
    Ok(connection
        .query_row(
            "SELECT value,claim_id,rank FROM ivm_heads WHERE view=?1 AND key=?2 AND register=?3",
            params![view, key, register],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

pub fn source_cut(connection: &Connection) -> Result<Option<SourceCut>> {
    Ok(connection
        .query_row(
            "SELECT epoch,admitted,projected,local_generation FROM ivm_source WHERE singleton=1",
            [],
            |row| {
                Ok(SourceCut {
                    epoch: row.get(0)?,
                    admitted: row.get(1)?,
                    projected: row.get(2)?,
                    local_generation: row.get(3)?,
                })
            },
        )
        .optional()?)
}

fn note_key_change(transaction: &Transaction<'_>, view: &str, key: &str) -> Result<()> {
    let sequence: u64 = transaction.query_row(
        "UPDATE ivm_key_frontier SET sequence=sequence+1 WHERE singleton=1 RETURNING sequence",
        [],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO ivm_keys VALUES(?1,?2,1,?3) ON CONFLICT(view,key) DO UPDATE SET generation=generation+1,changed_sequence=excluded.changed_sequence",
        params![view,key,sequence],
    )?;
    Ok(())
}

fn fence_error(transaction: &Transaction<'_>, view: &str, error: &anyhow::Error) -> Result<()> {
    transaction.execute("UPDATE ivm_views SET ready=0 WHERE name=?1", [view])?;
    let bounded = format!("{error:#}").chars().take(1024).collect::<String>();
    transaction.execute("INSERT INTO ivm_view_errors VALUES(?1,?2) ON CONFLICT(view) DO UPDATE SET error=excluded.error WHERE error<>excluded.error",params![view,bounded])?;
    Ok(())
}
