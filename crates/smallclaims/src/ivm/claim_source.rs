//! Opt-in retained admitted-claim source for namespace-aware installation.
//!
//! This adapter has the claims-only ViewRuntime admission policy. It retains repaired
//! originals, like canonical::components, and supplies no signer/authority closure, local
//! observations or ordered-run semantics. Runtime hooks capture appends/projection; direct
//! canonical mutations fence rather than recover on the writer. Registration, extraction
//! and recovery are explicit. There is no scan/index build at open or ordinary reads.

use super::{
    install::{Installer, Mutation, Root, ScanPage, SourcePosition, Status},
    source_cut,
};
use crate::{
    ClaimRecord,
    store::{canonical, claim_from_row, current_index},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

const LAYOUT: &str =
    "smallclaims.ivm.retained-claims.v2;plain-admission;retain-repaired;canonical.v1";
const COLUMNS: &str =
    "id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ivm_install_claim_kinds (
 source TEXT NOT NULL, kind TEXT NOT NULL, PRIMARY KEY(source,kind)
);
CREATE INDEX IF NOT EXISTS ivm_install_claim_kind_sources ON ivm_install_claim_kinds(kind,source);
CREATE TABLE IF NOT EXISTS ivm_install_claim_ranks (
 source TEXT NOT NULL, claim_id TEXT NOT NULL, position INTEGER NOT NULL,
 PRIMARY KEY(source,claim_id)
);
CREATE INDEX IF NOT EXISTS ivm_install_claim_rank_sources ON ivm_install_claim_ranks(claim_id,source);
";

/// Arrival index is extraction bookkeeping only. Rank is the exact canonical sortable tuple;
/// time remains a decimal string so the complete u128 domain is preserved in JSON.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClaimFact {
    pub id: String,
    pub batch_id: String,
    pub subject: String,
    pub kind: String,
    pub origin: String,
    pub actor: Option<String>,
    pub body: serde_json::Value,
    pub predecessors: Vec<String>,
    pub accepted_at_unix_ms: String,
    pub rank: Vec<u8>,
    pub canonical_position: u64,
}
impl ClaimFact {
    fn read(connection: &Connection, claim: &ClaimRecord) -> Result<Self> {
        let key = canonical::claim_key(connection, &claim.id)?;
        Ok(Self {
            id: claim.id.clone(),
            batch_id: claim.batch_id.clone(),
            subject: claim.subject.clone(),
            kind: claim.kind.clone(),
            origin: claim.origin.clone(),
            actor: claim.actor.clone(),
            body: claim.body.clone(),
            predecessors: claim.predecessors.clone(),
            accepted_at_unix_ms: claim.accepted_at_unix_ms.to_string(),
            rank: canonical::sortable_key(&key),
            canonical_position: key.4,
        })
    }
    fn mutation(self) -> Result<Mutation> {
        Ok(Mutation {
            key: self.id.clone(),
            old: None,
            new: Some(serde_json::to_value(self)?),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ClaimAvailability {
    pub installation: Status,
    pub cut: Option<super::SourceCut>,
    /// Shared committed pending/ready invalidation; independent of semantic generation.
    pub source_sequence: u64,
    pub ready: bool,
}

/// A finite exact-kind registry. Each operator must name this source, and must be independent
/// of signer metadata/local state: those inputs require a different, complete source adapter.
/// The file has one ViewRuntime owner; another runtime/raw SQL cannot attest hook coverage.
pub struct ClaimSource {
    pub installer: Arc<Installer>,
    name: &'static str,
    kinds: BTreeSet<&'static str>,
    fingerprint: String,
}
impl ClaimSource {
    pub fn new(
        installer: Arc<Installer>,
        name: &'static str,
        kinds: &[&'static str],
    ) -> Result<Self> {
        ensure!(
            !name.is_empty() && (1..=32).contains(&kinds.len()),
            "invalid retained claim registry"
        );
        let kinds: BTreeSet<_> = kinds.iter().copied().collect();
        ensure!(
            kinds
                .iter()
                .all(|kind| !kind.is_empty() && !kind.contains('*')),
            "exact claim kinds required"
        );
        use sha2::{Digest, Sha256};
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(LAYOUT, name, &kinds))?));
        Ok(Self {
            installer,
            name,
            kinds,
            fingerprint,
        })
    }
    pub fn name(&self) -> &'static str {
        self.name
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(crate) fn create_schema(&self, connection: &Connection) -> Result<()> {
        self.installer.create_schema(connection)?;
        connection.execute_batch(SCHEMA)?;
        // Indexed claim/kind discovery, bounded by registered sources, never by same-batch
        // children or output count. Unsupported canonical edits invalidate the entire source.
        let relevant = "SELECT DISTINCT k.source FROM ivm_install_claim_kinds k JOIN claims c ON c.kind=k.kind WHERE c.id=OLD.claim_id";
        let insert_relevant = "SELECT DISTINCT k.source FROM ivm_install_claim_kinds k JOIN claims c ON c.kind=k.kind WHERE c.id=NEW.claim_id";
        let all = "SELECT name FROM ivm_install_sources WHERE name IN (SELECT source FROM ivm_install_claim_kinds)";
        // Do not run another legacy COUNT for every replica record. A captured rank gives
        // an indexed comparison. Missing provenance fences already-projected history;
        // an uncaptured pending claim will read its final rank during projection. A stored
        // rank still fences changes even when a local append sits beyond the projected cut.
        // canonical::claim_key itself still has its existing legacy COUNT growth gate.
        let changed_sources = "SELECT k.source FROM ivm_install_claim_kinds k JOIN claims c ON c.kind=k.kind WHERE c.id=NEW.claim_id AND ((c.store_index<=(SELECT projected FROM ivm_source WHERE singleton=1) AND NOT EXISTS(SELECT 1 FROM ivm_install_claim_ranks r WHERE r.source=k.source AND r.claim_id=c.id)) OR (SELECT position FROM ivm_install_claim_ranks r WHERE r.source=k.source AND r.claim_id=c.id)<>(SELECT MIN(position) FROM replica_records WHERE claim_id=c.id))";
        // Replace the bounded trigger definition on upgrade; the v2 source fingerprint
        // keeps any prior registered source/namespace unavailable until explicit recovery.
        connection.execute_batch("DROP TRIGGER IF EXISTS ivm_install_claim_record_insert")?;
        let triggers: [(&str, &str, String, String); 7] = [
            ("record_insert", "AFTER INSERT ON replica_records", "NEW.claim_id IS NOT NULL".into(), changed_sources.to_string()),
            ("record_delete", "AFTER DELETE ON replica_records", "OLD.claim_id IS NOT NULL".into(), relevant.to_string()),
            ("record_update", "AFTER UPDATE OF position,claim_id ON replica_records", "OLD.position<>NEW.position OR OLD.claim_id IS NOT NEW.claim_id".into(), format!("{relevant} UNION {insert_relevant}")),
            // Even deletion of an unrelated kind can change the legacy COUNT ranks.
            ("claim_delete", "AFTER DELETE ON claims", "1".into(), all.to_string()),
            ("claim_update", "AFTER UPDATE OF id,batch_id,store_index,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms ON claims", "OLD.id<>NEW.id OR OLD.batch_id<>NEW.batch_id OR OLD.store_index<>NEW.store_index OR OLD.subject<>NEW.subject OR OLD.kind<>NEW.kind OR OLD.origin<>NEW.origin OR OLD.actor IS NOT NEW.actor OR OLD.body<>NEW.body OR OLD.predecessors<>NEW.predecessors OR OLD.accepted_at_unix_ms<>NEW.accepted_at_unix_ms".into(), all.to_string()),
            ("batch_update", "AFTER UPDATE OF id,origin,replica_sequence ON batches", "OLD.id<>NEW.id OR OLD.origin<>NEW.origin OR OLD.replica_sequence<>NEW.replica_sequence".into(), all.to_string()),
            ("source_epoch", "AFTER UPDATE OF epoch ON ivm_source", "OLD.epoch<>NEW.epoch".into(), all.to_string()),
        ];
        for (name, event, condition, sources) in triggers {
            connection.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS ivm_install_claim_{name} {event} WHEN {condition} BEGIN
                UPDATE ivm_install_sources SET available=0 WHERE name IN ({sources});
                UPDATE ivm_install_jobs SET phase='stopped',error='retained claim source changed outside bounded capture' WHERE source IN ({sources}) AND phase IN ('scan','catchup');
                UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error='retained claim source changed outside bounded capture' WHERE source IN ({sources}) AND (ready<>0 OR error IS NOT 'retained claim source changed outside bounded capture');
                END;"))?;
        }
        Ok(())
    }

    fn fresh(&self, connection: &Connection) -> Result<super::SourceCut> {
        let cut = source_cut(connection)?.context("retained claim source unavailable")?;
        ensure!(
            cut.admitted == cut.projected && cut.projected == current_index(connection)?,
            "retained claim admission pending projection"
        );
        Ok(cut)
    }
    /// Explicit attestation of exclusive runtime/hook coverage. Existing retained claims are
    /// extracted only by explicit pages after start; registration never enumerates history.
    pub fn register(&self, tx: &Transaction<'_>) -> Result<()> {
        let cut = self.fresh(tx)?;
        ensure!(
            !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ivm_install_claim_kinds WHERE source<>?1)",
                [self.name],
                |r| r.get::<_, bool>(0)
            )?,
            "one retained claim adapter source per runtime file"
        );
        self.installer
            .register_source(tx, self.name, &self.fingerprint, cut.epoch)?;
        for kind in &self.kinds {
            tx.execute(
                "INSERT INTO ivm_install_claim_kinds VALUES(?1,?2)",
                params![self.name, kind],
            )?;
        }
        Ok(())
    }
    /// Recovery is an explicit new lifecycle after canonical mutation/hook repair. Old roots
    /// remain fenced; extraction must calculate all current ranks again before publication.
    /// This attests claims-only coverage, never signer/authority/deadline completeness.
    pub fn restore(&self, tx: &Transaction<'_>, expected: &SourcePosition) -> Result<()> {
        ensure!(
            expected.source == self.name,
            "retained claim recovery source mismatch"
        );
        let cut = self.fresh(tx)?;
        self.installer
            .restore_source(tx, expected, &self.fingerprint, cut.epoch)?;
        tx.execute(
            "DELETE FROM ivm_install_claim_kinds WHERE source=?1",
            [self.name],
        )?;
        for kind in &self.kinds {
            tx.execute(
                "INSERT INTO ivm_install_claim_kinds VALUES(?1,?2)",
                params![self.name, kind],
            )?;
        }
        Ok(())
    }
    /// Runtime seam only; missing or fenced derived state cannot reject an admitted append.
    pub(crate) fn capture(&self, tx: &Transaction<'_>, claim: &ClaimRecord) -> Result<()> {
        if !self.kinds.contains(claim.kind.as_str()) {
            return Ok(());
        }
        let stored: Option<(String, u64, bool)> = tx
            .query_row(
                "SELECT fingerprint,epoch,available FROM ivm_install_sources WHERE name=?1",
                [self.name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((fingerprint, epoch, available)) = stored else {
            return Ok(());
        };
        let cut = source_cut(tx)?.context("retained claim source unavailable")?;
        if fingerprint != self.fingerprint || epoch != cut.epoch {
            self.installer.source_gap(
                tx,
                self.name,
                "retained claim adapter version or epoch mismatch",
            )?;
        }
        if !available || fingerprint != self.fingerprint || epoch != cut.epoch {
            // record advances the revision while unavailable without serializing/decode work.
            self.installer.record(
                tx,
                self.name,
                &Mutation {
                    key: claim.id.clone(),
                    old: None,
                    new: None,
                },
            )?;
            return Ok(());
        }
        let fact = ClaimFact::read(tx, claim)?;
        tx.execute("INSERT INTO ivm_install_claim_ranks VALUES(?1,?2,?3) ON CONFLICT(source,claim_id) DO UPDATE SET position=excluded.position",params![self.name,claim.id,fact.canonical_position])?;
        self.installer.record(tx, self.name, &fact.mutation()?)?;
        Ok(())
    }
    /// Capture this with output in one snapshot. Installer::root alone does not check the
    /// enclosing Store's admitted/projected frontier; callers of this adapter must use root.
    pub fn root(&self, connection: &Connection, view: &str) -> Result<Root> {
        let cut = self.fresh(connection)?;
        let position = self.installer.position(connection, self.name)?;
        ensure!(
            position.fingerprint == self.fingerprint && position.epoch == cut.epoch,
            "retained claim source incompatible"
        );
        // Prevent a view from a different source being read through this adapter.
        let source: String = connection.query_row(
            "SELECT source FROM ivm_install_roots WHERE view=?1",
            [view],
            |r| r.get(0),
        )?;
        ensure!(source == self.name, "retained claim root source mismatch");
        self.installer.root(connection, view)
    }
    /// Capture alongside predicate/output in one snapshot. Subscribe before capture and
    /// recheck after commit notification; the token is invalidation, never a durable event.
    pub fn availability(&self, connection: &Connection, view: &str) -> Result<ClaimAvailability> {
        let installation = self.installer.status(connection, view)?;
        ensure!(
            installation.source.source == self.name,
            "retained claim availability source mismatch"
        );
        let cut = source_cut(connection)?;
        let source_sequence = connection.query_row(
            "SELECT source_sequence FROM ivm_status_frontier WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let current = current_index(connection)?;
        let ready = installation.ready
            && installation.source.fingerprint == self.fingerprint
            && cut.as_ref().is_some_and(|cut| {
                cut.epoch == installation.source.epoch && cut.admitted == cut.projected
            })
            && cut.as_ref().is_some_and(|cut| current == cut.projected);
        Ok(ClaimAvailability {
            installation,
            cut,
            source_sequence,
            ready,
        })
    }
    /// One short caller-owned read snapshot, released before Installer::scan. Uses existing
    /// claims_kind_index; opens and registration never construct an index over old claims.
    /// The fixed-width cursor is an arrival position, fenced on remap, not canonical order.
    pub fn extract(
        &self,
        connection: &Connection,
        job: &str,
        rows: usize,
        bytes: usize,
    ) -> Result<ScanPage> {
        ensure!(
            (1..=128).contains(&rows) && (1..=1024 * 1024).contains(&bytes),
            "retained claim extraction bound invalid"
        );
        let cut = self.fresh(connection)?;
        let position = self.installer.position(connection, self.name)?;
        ensure!(
            position.fingerprint == self.fingerprint && position.epoch == cut.epoch,
            "retained claim extraction source incompatible"
        );
        let progress = self.installer.progress(connection, job)?;
        let source: String = connection.query_row(
            "SELECT source FROM ivm_install_jobs WHERE id=?1",
            [job],
            |r| r.get(0),
        )?;
        ensure!(
            source == self.name && progress.phase == "scan",
            "retained claim extraction job mismatch"
        );
        let cursor = if progress.cursor.is_empty() {
            0
        } else {
            u64::from_be_bytes(
                progress
                    .cursor
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("retained claim cursor incompatible"))?,
            )
        };
        let mut candidates = BTreeSet::new();
        for kind in &self.kinds {
            let mut statement=connection.prepare_cached("SELECT store_index FROM claims INDEXED BY claims_kind_index WHERE kind=?1 AND store_index>?2 AND store_index<=?3 ORDER BY store_index LIMIT ?4")?;
            for index in statement.query_map(
                params![kind, cursor, cut.projected, (rows + 1) as u64],
                |r| r.get::<_, u64>(0),
            )? {
                candidates.insert(index?);
            }
        }
        let mut output = Vec::new();
        let mut next = cursor;
        let mut used = 0;
        for index in candidates.iter().take(rows) {
            // Inspect stored payload bytes before allocating/decoding a large source value.
            let size: u64=connection.query_row("SELECT length(CAST(body AS BLOB))+length(CAST(predecessors AS BLOB))+length(CAST(id AS BLOB))+length(CAST(batch_id AS BLOB))+length(CAST(subject AS BLOB))+length(CAST(kind AS BLOB))+length(CAST(origin AS BLOB))+COALESCE(length(CAST(actor AS BLOB)),0) FROM claims WHERE store_index=?1",[index],|r|r.get(0))?;
            if size > bytes as u64 {
                ensure!(
                    !output.is_empty(),
                    "retained claim exceeds extraction byte bound"
                );
                break;
            }
            let claim = connection.query_row(
                &format!("SELECT {COLUMNS} FROM claims WHERE store_index=?1"),
                [index],
                claim_from_row,
            )?;
            let mutation = ClaimFact::read(connection, &claim)?.mutation()?;
            let encoded = serde_json::to_vec(&mutation)?.len();
            if encoded > bytes - used {
                ensure!(
                    !output.is_empty(),
                    "retained claim exceeds encoded extraction byte bound"
                );
                break;
            }
            used += encoded;
            output.push(mutation);
            next = *index;
        }
        let finished = candidates.len() == output.len();
        Ok(ScanPage {
            job: job.into(),
            expected_cursor: progress.cursor,
            next_cursor: if next == cursor {
                if cursor == 0 {
                    Vec::new()
                } else {
                    cursor.to_be_bytes().to_vec()
                }
            } else {
                next.to_be_bytes().to_vec()
            },
            position,
            rows: output,
            finished,
        })
    }
}
