//! Explicit, resumable installation into an unpublished operator namespace.
//!
//! This module never reads claim history, runs at open, or supplies an admission policy.
//! A versioned source adapter owns indexed extraction and complete old/new mutation capture
//! (including repair, signatures, local replacements and same-index dependencies). Every
//! admitted mutation must call `record` in its source transaction. This is a separate,
//! opt-in operator API: legacy `View` callbacks with hardcoded tables are not installable.
//!
//! Scan pages release their read snapshot before taking the writer. All scan pages finish
//! before journal catch-up. Operators apply complete replacements idempotently, so an
//! extraction page that observed newer data is corrected by the ordered mutation journal.
//! Publication switches one namespace pointer after an O(1) completeness check. Namespace
//! reclamation and journal reclamation are explicit bounded jobs, never publication work.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const LAYOUT: &str = "smallclaims.ivm.install.v1";
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS ivm_install_sources (
 name TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL,
 revision INTEGER NOT NULL, available INTEGER NOT NULL,
 journal_rows INTEGER NOT NULL DEFAULT 0, journal_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS ivm_install_roots (
 view TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, namespace TEXT,
 source TEXT NOT NULL, source_fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL, revision INTEGER NOT NULL,
 ready INTEGER NOT NULL, generation INTEGER NOT NULL, status_revision INTEGER NOT NULL,
 error TEXT
);
CREATE INDEX IF NOT EXISTS ivm_install_source_roots ON ivm_install_roots(source,ready,view);
CREATE TABLE IF NOT EXISTS ivm_install_jobs (
 id TEXT PRIMARY KEY, view TEXT NOT NULL, fingerprint TEXT NOT NULL,
 source TEXT NOT NULL, source_fingerprint TEXT NOT NULL, epoch INTEGER NOT NULL,
 base_revision INTEGER NOT NULL, applied_revision INTEGER NOT NULL,
 cursor BLOB NOT NULL, phase TEXT NOT NULL, error TEXT,
 queued_rows INTEGER NOT NULL, queued_bytes INTEGER NOT NULL,
 limits TEXT NOT NULL, started_ms INTEGER NOT NULL,
 pages INTEGER NOT NULL DEFAULT 0, extracted_rows INTEGER NOT NULL DEFAULT 0,
 applied_rows INTEGER NOT NULL DEFAULT 0, max_page_us INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS ivm_install_active_view ON ivm_install_jobs(view)
 WHERE phase IN ('scan','catchup');
CREATE INDEX IF NOT EXISTS ivm_install_source_jobs ON ivm_install_jobs(source,phase,applied_revision);
CREATE INDEX IF NOT EXISTS ivm_install_view_jobs ON ivm_install_jobs(view,id);
CREATE TABLE IF NOT EXISTS ivm_install_journal (
 source TEXT NOT NULL, revision INTEGER NOT NULL, payload TEXT NOT NULL, bytes INTEGER NOT NULL,
 PRIMARY KEY(source,revision)
);
"#;

/// Namespace must be included in every operator-owned PK/index and every query predicate.
/// An operator receives this context on ordinary maintenance as well as installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespace(String);
impl Namespace {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Full replacement of a stable source key, not a coalesced invalidation or action event.
/// Source adapters encode exact authority, canonical rank and local measurement identity
/// in the values. A changed identity must retract the old key and insert the new one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mutation {
    pub key: String,
    pub old: Option<Value>,
    pub new: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePosition {
    pub source: String,
    pub fingerprint: String,
    pub epoch: u64,
    pub revision: u64,
}

pub trait Operator: Send + Sync {
    fn name(&self) -> &'static str;
    /// Includes schema, input registry, admission/authority/dependency and extractor versions.
    fn fingerprint(&self) -> &'static str;
    fn source(&self) -> &'static str;
    fn create_schema(&self, connection: &Connection) -> Result<()>;
    /// Must be bounded by the page and indexed affected outputs, include namespace in SQL,
    /// and roll back normally. Return visible membership/order/authority/content change.
    fn apply(&self, tx: &Transaction<'_>, namespace: &Namespace, rows: &[Mutation])
    -> Result<bool>;
    /// Indexed, bounded counters/evidence only: no full scan or history rebuild here.
    fn validate_publication(&self, tx: &Transaction<'_>, namespace: &Namespace) -> Result<()>;
    /// Delete at most `rows` across ALL namespace-owned tables using namespace indexes.
    /// Return true only when no state remains. No ready namespace is passed here.
    fn reclaim(&self, tx: &Transaction<'_>, namespace: &Namespace, rows: usize) -> Result<bool>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub page_rows: usize,
    pub page_bytes: usize,
    pub pending_rows: u64,
    pub pending_bytes: u64,
    /// Total extracted plus journal-applied rows; prevents endless catch-up even when its
    /// instantaneous backlog stays small. Exceeding requires an explicit operator decision.
    pub total_rows: u64,
    /// Elapsed callback budget. Crossing stops the job and rolls back that page. SQLite
    /// statements, commit/fsync and arbitrary callbacks are not preempted by this check.
    pub callback_ms: u64,
    /// Stop, never automatically restart, when concurrent writes prevent completion.
    pub lifetime_ms: u64,
}
impl Limits {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=1024).contains(&self.page_rows),
            "invalid installation page row bound"
        );
        ensure!(
            (1..=1024 * 1024).contains(&self.page_bytes),
            "invalid installation page byte bound"
        );
        ensure!(
            self.pending_rows > 0 && self.pending_rows <= i64::MAX as u64,
            "invalid journal row quota"
        );
        ensure!(
            self.pending_bytes > 0 && self.pending_bytes <= i64::MAX as u64,
            "invalid journal byte quota"
        );
        ensure!(
            self.total_rows > 0 && self.total_rows <= i64::MAX as u64,
            "invalid installation total work quota"
        );
        ensure!(
            (1..=1000).contains(&self.callback_ms)
                && self.lifetime_ms > 0
                && self.lifetime_ms <= i64::MAX as u64,
            "invalid installation time budget"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ScanPage {
    pub job: String,
    pub expected_cursor: Vec<u8>,
    pub next_cursor: Vec<u8>,
    /// Captured with the rows in one short source read snapshot, released before `scan`.
    pub position: SourcePosition,
    pub rows: Vec<Mutation>,
    /// Attests complete indexed extraction through the adapter's retained source boundary.
    pub finished: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Progress,
    Published,
    Stopped(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub namespace: Namespace,
    pub epoch: u64,
    pub revision: u64,
    pub generation: u64,
    pub status_revision: u64,
}

/// Readable while fenced, independently of output readiness. Capture with actual output
/// in one snapshot; commit notification/subscribe-before-recheck is the adapter's seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub source: SourcePosition,
    pub source_available: bool,
    pub compatible: bool,
    pub ready: bool,
    pub generation: u64,
    pub status_revision: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub id: String,
    pub view: String,
    pub phase: String,
    pub error: Option<String>,
    pub cursor: Vec<u8>,
    pub base_revision: u64,
    pub applied_revision: u64,
    pub queued_rows: u64,
    pub queued_bytes: u64,
    pub pages: u64,
    pub extracted_rows: u64,
    pub applied_rows: u64,
    pub max_page_us: u64,
}

struct Job {
    progress: Progress,
    fingerprint: String,
    source: String,
    source_fingerprint: String,
    epoch: u64,
    limits: Limits,
    started_ms: u64,
}

pub struct Installer {
    operators: BTreeMap<&'static str, Box<dyn Operator>>,
    fingerprints: BTreeMap<&'static str, String>,
}
impl Installer {
    pub fn new(operators: Vec<Box<dyn Operator>>) -> Result<Self> {
        let mut result = Self {
            operators: BTreeMap::new(),
            fingerprints: BTreeMap::new(),
        };
        for operator in operators {
            ensure!(
                !operator.name().is_empty()
                    && !operator.source().is_empty()
                    && !operator.fingerprint().is_empty(),
                "empty install operator identity"
            );
            let name = operator.name();
            use sha2::{Digest, Sha256};
            let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
                LAYOUT,
                name,
                operator.source(),
                operator.fingerprint(),
            ))?));
            ensure!(
                result.operators.insert(name, operator).is_none(),
                "duplicate install operator"
            );
            result.fingerprints.insert(name, fingerprint);
        }
        Ok(result)
    }

    pub(super) fn binding_definition(&self, view: &str) -> Result<(&str, &str, &str)> {
        let operator = self
            .operators
            .get(view)
            .context("unknown installation operator")?;
        Ok((
            operator.source(),
            operator.fingerprint(),
            &self.fingerprints[view],
        ))
    }

    pub(super) fn expected_fingerprint(view: &str, source: &str, raw: &str) -> Result<String> {
        use sha2::{Digest, Sha256};
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            LAYOUT, view, source, raw,
        ))?)))
    }

    pub fn create_schema(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(SCHEMA)?;
        for operator in self.operators.values() {
            operator.create_schema(connection)?;
        }
        Ok(())
    }

    /// Explicit source registration. The caller must prove complete notification/extraction
    /// coverage before registering an available source. Reopen does not register or heal it.
    pub fn register_source(
        &self,
        tx: &Transaction<'_>,
        name: &str,
        fingerprint: &str,
        epoch: u64,
    ) -> Result<()> {
        ensure!(
            !name.is_empty() && !fingerprint.is_empty() && epoch <= i64::MAX as u64,
            "invalid install source identity"
        );
        tx.execute("INSERT INTO ivm_install_sources(name,fingerprint,epoch,revision,available) VALUES(?1,?2,?3,0,1)", params![name,fingerprint,epoch])?;
        Ok(())
    }

    pub fn position(&self, connection: &Connection, source: &str) -> Result<SourcePosition> {
        let (fingerprint, epoch, revision, available): (String,u64,u64,bool) = connection.query_row(
            "SELECT fingerprint,epoch,revision,available FROM ivm_install_sources WHERE name=?1",
            [source], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(available, "installation source coverage unavailable");
        Ok(SourcePosition {
            source: source.into(),
            fingerprint,
            epoch,
            revision,
        })
    }

    /// Fence on an uncaptured mutation, coverage loss, source restore or incompatible version.
    /// This does not read/replay source rows and never admits an old namespace under a new cut.
    pub fn source_gap(&self, tx: &Transaction<'_>, source: &str, reason: &str) -> Result<()> {
        let reason = bounded_error(reason);
        tx.execute(
            "UPDATE ivm_install_sources SET available=0 WHERE name=?1",
            [source],
        )?;
        tx.execute("UPDATE ivm_install_jobs SET phase='stopped',error=?2 WHERE source=?1 AND phase IN ('scan','catchup')", params![source,reason])?;
        tx.execute("UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error=?2 WHERE source=?1 AND (ready<>0 OR error IS NOT ?2)",params![source,reason])?;
        Ok(())
    }

    /// Explicit owner attestation after repairing source coverage. Revision never decreases;
    /// every old root/job remains fenced. A changed epoch/fingerprint requires a new scan.
    pub fn restore_source(
        &self,
        tx: &Transaction<'_>,
        expected: &SourcePosition,
        fingerprint: &str,
        epoch: u64,
    ) -> Result<()> {
        ensure!(
            !fingerprint.is_empty() && epoch <= i64::MAX as u64,
            "invalid installation source replacement"
        );
        let (old, _) = source_position(tx, &expected.source)?;
        ensure!(&old == expected, "installation source recovery gap");
        self.source_gap(
            tx,
            &expected.source,
            "source coverage restored; explicit installation required",
        )?;
        tx.execute(
            "UPDATE ivm_install_sources SET fingerprint=?2,epoch=?3,available=1 WHERE name=?1",
            params![expected.source, fingerprint, epoch],
        )?;
        Ok(())
    }

    /// Complete source changes and live namespace maintenance share the source transaction.
    /// Journal/callback quotas fence derived work; valid source admission is preserved.
    /// Database/transaction failures still propagate: they cannot be disguised as admission.
    pub fn record(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        mutation: &Mutation,
    ) -> Result<SourcePosition> {
        let (mut position, available) = source_position(tx, source)?;
        position.revision = position
            .revision
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .context("installation source revision overflow")?;
        tx.execute(
            "UPDATE ivm_install_sources SET revision=?2 WHERE name=?1",
            params![source, position.revision],
        )?;
        if !available {
            return Ok(position);
        }
        if mutation.key.is_empty() {
            self.source_gap(
                tx,
                source,
                "empty source mutation key; coverage unavailable",
            )?;
            return Ok(position);
        }
        let payload = serde_json::to_string(mutation)?;
        let bytes = payload.len() as u64;
        let (retained_rows, retained_bytes): (u64, u64) = tx.query_row(
            "SELECT journal_rows,journal_bytes FROM ivm_install_sources WHERE name=?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // One shared journal row per source mutation. Job count is bounded by the registry,
        // never by graph history. Excess payload/quota stops jobs before copying the payload.
        let mut statement = tx.prepare_cached("SELECT id,limits,queued_rows,queued_bytes FROM ivm_install_jobs WHERE source=?1 AND phase IN ('scan','catchup')")?;
        let jobs = statement
            .query_map([source], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, u64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut journal = false;
        for (id, limits, rows, size) in jobs {
            let limits: Limits = serde_json::from_str(&limits)?;
            if bytes > limits.page_bytes as u64
                || rows >= limits.pending_rows
                || bytes > limits.pending_bytes.saturating_sub(size)
                || retained_rows >= limits.pending_rows
                || bytes > limits.pending_bytes.saturating_sub(retained_bytes)
            {
                stop(
                    tx,
                    &id,
                    "installation journal quota exceeded; explicit restart required",
                )?;
            } else {
                tx.execute("UPDATE ivm_install_jobs SET queued_rows=queued_rows+1,queued_bytes=queued_bytes+?2 WHERE id=?1", params![id,bytes])?;
                journal = true;
            }
        }
        if journal {
            tx.execute(
                "INSERT INTO ivm_install_journal VALUES(?1,?2,?3,?4)",
                params![source, position.revision, payload, bytes],
            )?;
            tx.execute("UPDATE ivm_install_sources SET journal_rows=journal_rows+1,journal_bytes=journal_bytes+?2 WHERE name=?1",params![source,bytes])?;
        }
        // Published namespaces use the same scoped operator, not legacy unscoped callbacks.
        let mut statement = tx.prepare_cached("SELECT view,namespace,fingerprint,epoch,source_fingerprint FROM ivm_install_roots WHERE source=?1 AND ready=1")?;
        let roots = statement
            .query_map([source], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for (view, namespace, fingerprint, epoch, source_fingerprint) in roots {
            tx.execute_batch("SAVEPOINT ivm_install_live")?;
            let result = (|| {
                let operator = self
                    .operators
                    .get(view.as_str())
                    .context("install operator no longer registered")?;
                ensure!(
                    self.fingerprints.get(view.as_str()) == Some(&fingerprint)
                        && operator.source() == source
                        && epoch == position.epoch
                        && source_fingerprint == position.fingerprint,
                    "live install namespace version mismatch"
                );
                operator.apply(tx, &Namespace(namespace), std::slice::from_ref(mutation))
            })();
            match result {
                Ok(changed) => {
                    tx.execute("UPDATE ivm_install_roots SET revision=?2,generation=generation+?3 WHERE view=?1",params![view,position.revision,changed])?;
                    tx.execute_batch("RELEASE ivm_install_live")?;
                }
                Err(error) => {
                    tx.execute_batch("ROLLBACK TO ivm_install_live; RELEASE ivm_install_live")?;
                    tx.execute("UPDATE ivm_install_roots SET ready=0,status_revision=status_revision+1,error=?2 WHERE view=?1",params![view,bounded_error(&format!("{error:#}"))])?;
                }
            }
        }
        Ok(position)
    }

    /// Explicit operator action. An existing compatible ready namespace remains live while
    /// a replacement builds; a missing/incompatible root stays unready. No old rows are deleted.
    pub fn start(
        &self,
        tx: &Transaction<'_>,
        view: &str,
        limits: Limits,
        now_ms: u64,
    ) -> Result<String> {
        limits.validate()?;
        ensure!(now_ms <= i64::MAX as u64, "installation time out of range");
        let operator = self
            .operators
            .get(view)
            .context("unknown installation operator")?;
        let position = self.position(tx, operator.source())?;
        let fingerprint = &self.fingerprints[view];
        let leftovers: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_jobs j WHERE view=?1 AND
            NOT EXISTS(SELECT 1 FROM ivm_install_roots r WHERE r.namespace=j.id))",
            [view],
            |r| r.get(0),
        )?;
        ensure!(
            !leftovers,
            "reclaim previous installation namespace before restarting"
        );
        let (rows, bytes): (u64, u64) = tx.query_row(
            "SELECT journal_rows,journal_bytes FROM ivm_install_sources WHERE name=?1",
            [&position.source],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            rows <= limits.pending_rows && bytes <= limits.pending_bytes,
            "prune retained installation journal before starting"
        );
        tx.execute("INSERT INTO ivm_install_roots(view,fingerprint,namespace,source,source_fingerprint,epoch,revision,ready,generation,status_revision,error)
            VALUES(?1,?2,NULL,?3,?4,?5,?6,0,0,1,NULL)
            ON CONFLICT(view) DO UPDATE SET ready=CASE WHEN fingerprint=excluded.fingerprint AND epoch=excluded.epoch
                AND source_fingerprint=excluded.source_fingerprint THEN ready ELSE 0 END,
            status_revision=status_revision+1",params![view,fingerprint,position.source,position.fingerprint,position.epoch,position.revision])?;
        let id = uuid::Uuid::now_v7().to_string();
        tx.execute("INSERT INTO ivm_install_jobs(id,view,fingerprint,source,source_fingerprint,epoch,base_revision,applied_revision,cursor,phase,error,queued_rows,queued_bytes,limits,started_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?7,?8,'scan',NULL,0,0,?9,?10)",
            params![id,view,fingerprint,position.source,position.fingerprint,position.epoch,position.revision,Vec::<u8>::new(),serde_json::to_string(&limits)?,now_ms])?;
        Ok(id)
    }

    pub fn cancel(&self, tx: &Transaction<'_>, job: &str) -> Result<()> {
        stop(tx, job, "installation cancelled; explicit restart required")
    }

    pub fn progress(&self, connection: &Connection, job: &str) -> Result<Progress> {
        Ok(load_job(connection, job)?.progress)
    }

    fn check<'a>(
        &'a self,
        tx: &Transaction<'_>,
        job: &Job,
        now_ms: u64,
    ) -> Result<&'a dyn Operator> {
        ensure!(
            matches!(job.progress.phase.as_str(), "scan" | "catchup"),
            "installation not active"
        );
        ensure!(
            now_ms >= job.started_ms && now_ms - job.started_ms <= job.limits.lifetime_ms,
            "installation lifetime exceeded or clock moved backwards; explicit restart required"
        );
        let operator = self
            .operators
            .get(job.progress.view.as_str())
            .context("installation operator removed")?;
        ensure!(
            self.fingerprints.get(job.progress.view.as_str()) == Some(&job.fingerprint),
            "installation operator version changed; explicit restart required"
        );
        let source = self.position(tx, &job.source)?;
        ensure!(
            source.epoch == job.epoch && source.fingerprint == job.source_fingerprint,
            "installation source identity changed; explicit restart required"
        );
        Ok(operator.as_ref())
    }

    pub fn scan(&self, tx: &Transaction<'_>, page: &ScanPage, now_ms: u64) -> Result<Outcome> {
        let job = load_job(tx, &page.job)?;
        let started = Instant::now();
        tx.execute_batch("SAVEPOINT ivm_install_page")?;
        let result = (|| -> Result<()> {
            let operator = self.check(tx, &job, now_ms)?;
            ensure!(
                job.progress.phase == "scan" && page.expected_cursor == job.progress.cursor,
                "installation page cursor mismatch"
            );
            ensure!(
                page.next_cursor > page.expected_cursor
                    || (page.finished
                        && page.rows.is_empty()
                        && page.next_cursor == page.expected_cursor),
                "installation scan made no progress"
            );
            let current = self.position(tx, &job.source)?;
            ensure!(
                page.position.source == current.source
                    && page.position.epoch == current.epoch
                    && page.position.fingerprint == current.fingerprint
                    && page.position.revision >= job.progress.base_revision
                    && page.position.revision <= current.revision,
                "installation page source cut mismatch"
            );
            validate_page(&job.limits, &page.rows)?;
            validate_total_work(&job, page.rows.len())?;
            // Extraction rows are complete current values, not old/new change events.
            ensure!(
                page.rows.iter().all(|r| r.old.is_none() && r.new.is_some()),
                "scan requires current source replacements"
            );
            operator.apply(tx, &Namespace(page.job.clone()), &page.rows)?;
            ensure!(
                started.elapsed() <= Duration::from_millis(job.limits.callback_ms),
                "installation page callback budget exceeded"
            );
            tx.execute("UPDATE ivm_install_jobs SET cursor=?2,phase=?3,pages=pages+1,extracted_rows=extracted_rows+?4,max_page_us=MAX(max_page_us,?5) WHERE id=?1",
                params![page.job,page.next_cursor,if page.finished {"catchup"} else {"scan"},page.rows.len() as u64,elapsed_us(started)])?;
            Ok(())
        })();
        finish_page(tx, &page.job, result, Outcome::Progress)
    }

    /// Apply at most one bounded journal page; the caller yields/releases the writer between
    /// calls. No long read snapshot or transaction spans these calls or publication.
    pub fn catch_up(&self, tx: &Transaction<'_>, id: &str, now_ms: u64) -> Result<Outcome> {
        let job = load_job(tx, id)?;
        let started = Instant::now();
        tx.execute_batch("SAVEPOINT ivm_install_page")?;
        let result = (|| -> Result<Outcome> {
            let operator = self.check(tx, &job, now_ms)?;
            ensure!(
                job.progress.phase == "catchup",
                "installation extraction incomplete"
            );
            let current = self.position(tx, &job.source)?;
            if job.progress.applied_revision == current.revision {
                ensure!(
                    job.progress.queued_rows == 0 && job.progress.queued_bytes == 0,
                    "installation journal counters disagree with publication cut"
                );
                operator.validate_publication(tx, &Namespace(id.into()))?;
                ensure!(
                    self.position(tx, &job.source)? == current,
                    "source changed during installation validation"
                );
                ensure!(
                    started.elapsed() <= Duration::from_millis(job.limits.callback_ms),
                    "installation publication callback budget exceeded"
                );
                tx.execute("UPDATE ivm_install_roots SET namespace=?2,fingerprint=?3,epoch=?4,revision=?5,source_fingerprint=?6,source=?7,ready=1,
                    generation=generation+1,status_revision=status_revision+1,error=NULL WHERE view=?1",
                    params![job.progress.view,id,job.fingerprint,current.epoch,current.revision,current.fingerprint,current.source])?;
                tx.execute(
                    "UPDATE ivm_install_jobs SET phase='published' WHERE id=?1",
                    [id],
                )?;
                return Ok(Outcome::Published);
            }
            let mut statement = tx.prepare_cached("SELECT revision,payload,bytes FROM ivm_install_journal WHERE source=?1 AND revision>?2 ORDER BY revision LIMIT ?3")?;
            let mut entries = statement.query(params![
                job.source,
                job.progress.applied_revision,
                job.limits.page_rows as u64
            ])?;
            let mut rows = Vec::new();
            let mut bytes = 0;
            let mut through = job.progress.applied_revision;
            while let Some(entry) = entries.next()? {
                let revision: u64 = entry.get(0)?;
                let size: u64 = entry.get(2)?;
                ensure!(revision == through + 1, "installation mutation journal gap");
                if bytes + size > job.limits.page_bytes as u64 {
                    break;
                }
                let payload: String = entry.get(1)?;
                ensure!(
                    payload.len() as u64 == size,
                    "installation journal byte counter mismatch"
                );
                rows.push(serde_json::from_str::<Mutation>(&payload)?);
                bytes += size;
                through = revision;
            }
            drop(entries);
            drop(statement);
            ensure!(
                !rows.is_empty(),
                "installation mutation journal gap or oversized entry"
            );
            validate_page(&job.limits, &rows)?;
            validate_total_work(&job, rows.len())?;
            operator.apply(tx, &Namespace(id.into()), &rows)?;
            ensure!(
                started.elapsed() <= Duration::from_millis(job.limits.callback_ms),
                "installation catch-up callback budget exceeded"
            );
            tx.execute("UPDATE ivm_install_jobs SET applied_revision=?2,queued_rows=queued_rows-?3,queued_bytes=queued_bytes-?4,
                pages=pages+1,applied_rows=applied_rows+?3,max_page_us=MAX(max_page_us,?5) WHERE id=?1",
                params![id,through,rows.len() as u64,bytes,elapsed_us(started)])?;
            Ok(Outcome::Progress)
        })();
        match result {
            Ok(outcome) => finish_page(tx, id, Ok(()), outcome),
            Err(error) => finish_page(tx, id, Err(error), Outcome::Progress),
        }
    }

    /// Capture with actual output in one authoritative read snapshot. Root/status revisions
    /// invalidate namespaces; they do not grant authorization, historical cuts or action replay.
    pub fn root(&self, connection: &Connection, view: &str) -> Result<Root> {
        let (namespace,fingerprint,source,epoch,revision,ready,generation,status,source_fingerprint): (Option<String>,String,String,u64,u64,bool,u64,u64,String) = connection.query_row(
            "SELECT namespace,fingerprint,source,epoch,revision,ready,generation,status_revision,source_fingerprint FROM ivm_install_roots WHERE view=?1",[view],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?;
        ensure!(
            self.fingerprints.get(view) == Some(&fingerprint)
                && ready
                && self
                    .operators
                    .get(view)
                    .is_some_and(|operator| operator.source() == source),
            "installed view unready or incompatible"
        );
        let position = self.position(connection, &source)?;
        ensure!(
            position.epoch == epoch
                && position.revision == revision
                && position.fingerprint == source_fingerprint,
            "installed view source pending or replaced"
        );
        Ok(Root {
            namespace: Namespace(namespace.context("installed view has no namespace")?),
            epoch,
            revision,
            generation,
            status_revision: status,
        })
    }

    pub fn status(&self, connection: &Connection, view: &str) -> Result<Status> {
        let (fingerprint,source,epoch,revision,ready,generation,status,error,source_fingerprint): (String,String,u64,u64,bool,u64,u64,Option<String>,String) = connection.query_row(
            "SELECT fingerprint,source,epoch,revision,ready,generation,status_revision,error,source_fingerprint FROM ivm_install_roots WHERE view=?1",[view],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)))?;
        let (position, available) = source_position(connection, &source)?;
        let compatible = self.fingerprints.get(view) == Some(&fingerprint)
            && self
                .operators
                .get(view)
                .is_some_and(|operator| operator.source() == source)
            && epoch == position.epoch
            && source_fingerprint == position.fingerprint;
        Ok(Status {
            ready: ready && available && compatible && revision == position.revision,
            source: position,
            source_available: available,
            compatible,
            generation,
            status_revision: status,
            error,
        })
    }

    /// Explicit bounded reclamation, below every active job's consumed journal revision.
    /// Stopped jobs cannot resume and do not pin the journal; no deletion runs on source append.
    pub fn prune_journal(&self, tx: &Transaction<'_>, source: &str, rows: usize) -> Result<usize> {
        ensure!(
            (1..=1024).contains(&rows),
            "invalid journal reclamation page"
        );
        let position = self.position(tx, source)?;
        let through: u64 = tx.query_row("SELECT COALESCE(MIN(applied_revision),?2) FROM ivm_install_jobs WHERE source=?1 AND phase IN ('scan','catchup')",
            params![source,position.revision],|r| r.get(0))?;
        let mut statement = tx.prepare_cached("SELECT revision,bytes FROM ivm_install_journal WHERE source=?1 AND revision<=?2 ORDER BY revision LIMIT ?3")?;
        let entries = statement
            .query_map(params![source, through, rows as u64], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut bytes = 0;
        for (revision, size) in &entries {
            tx.execute(
                "DELETE FROM ivm_install_journal WHERE source=?1 AND revision=?2",
                params![source, revision],
            )?;
            bytes += size;
        }
        tx.execute("UPDATE ivm_install_sources SET journal_rows=journal_rows-?2,journal_bytes=journal_bytes-?3 WHERE name=?1",params![source,entries.len() as u64,bytes])?;
        Ok(entries.len())
    }

    /// Explicit namespace cleanup. A completed old namespace must be reclaimed before a
    /// third namespace can start, preventing repeated failures/restarts from accumulating
    /// unlimited shadow copies. Operator/schema upgrades need a compatible old reclaimer.
    pub fn reclaim(&self, tx: &Transaction<'_>, id: &str, rows: usize) -> Result<bool> {
        ensure!(
            (1..=1024).contains(&rows),
            "invalid namespace reclamation page"
        );
        let job = load_job(tx, id)?;
        ensure!(
            matches!(job.progress.phase.as_str(), "stopped" | "published"),
            "cannot reclaim active installation"
        );
        let attached: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ivm_install_roots WHERE namespace=?1)",
            [id],
            |r| r.get(0),
        )?;
        ensure!(!attached, "cannot reclaim attached namespace");
        ensure!(
            self.fingerprints.get(job.progress.view.as_str()) == Some(&job.fingerprint),
            "old installation needs its compatible namespace reclaimer"
        );
        let operator = self
            .operators
            .get(job.progress.view.as_str())
            .context("namespace reclaimer unavailable")?;
        let done = operator.reclaim(tx, &Namespace(id.into()), rows)?;
        if done {
            tx.execute("DELETE FROM ivm_install_jobs WHERE id=?1", [id])?;
        }
        Ok(done)
    }
}

fn source_position(connection: &Connection, source: &str) -> Result<(SourcePosition, bool)> {
    Ok(connection.query_row(
        "SELECT fingerprint,epoch,revision,available FROM ivm_install_sources WHERE name=?1",
        [source],
        |r| {
            Ok((
                SourcePosition {
                    source: source.into(),
                    fingerprint: r.get(0)?,
                    epoch: r.get(1)?,
                    revision: r.get(2)?,
                },
                r.get(3)?,
            ))
        },
    )?)
}

fn validate_page(limits: &Limits, rows: &[Mutation]) -> Result<()> {
    ensure!(
        rows.len() <= limits.page_rows,
        "installation page row bound exceeded"
    );
    let mut bytes = 0usize;
    for row in rows {
        ensure!(!row.key.is_empty(), "empty installation source key");
        bytes = bytes
            .checked_add(serde_json::to_vec(row)?.len())
            .context("installation page byte overflow")?;
        ensure!(
            bytes <= limits.page_bytes,
            "installation page byte bound exceeded"
        );
    }
    Ok(())
}
fn validate_total_work(job: &Job, rows: usize) -> Result<()> {
    ensure!(
        job.progress
            .extracted_rows
            .checked_add(job.progress.applied_rows)
            .and_then(|r| r.checked_add(rows as u64))
            .is_some_and(|r| r <= job.limits.total_rows),
        "installation total work quota exceeded; explicit restart required"
    );
    Ok(())
}
fn elapsed_us(start: Instant) -> u64 {
    start.elapsed().as_micros().min(i64::MAX as u128) as u64
}
fn bounded_error(reason: &str) -> String {
    reason.chars().take(1024).collect()
}
fn stop(tx: &Transaction<'_>, id: &str, reason: &str) -> Result<()> {
    ensure!(
        mark_stopped(tx, id, reason)? == 1,
        "installation not active"
    );
    Ok(())
}
fn mark_stopped(tx: &Transaction<'_>, id: &str, reason: &str) -> Result<usize> {
    let reason = bounded_error(reason);
    let count = tx.execute("UPDATE ivm_install_jobs SET phase='stopped',error=?2 WHERE id=?1 AND phase IN ('scan','catchup')",params![id,reason])?;
    if count != 0 {
        tx.execute("UPDATE ivm_install_roots SET error=?2,status_revision=status_revision+1
            WHERE view=(SELECT view FROM ivm_install_jobs WHERE id=?1) AND ready=0 AND error IS NOT ?2",params![id,reason])?;
    }
    Ok(count)
}
fn finish_page(
    tx: &Transaction<'_>,
    id: &str,
    result: Result<()>,
    outcome: Outcome,
) -> Result<Outcome> {
    match result {
        Ok(()) => {
            tx.execute_batch("RELEASE ivm_install_page")?;
            Ok(outcome)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO ivm_install_page; RELEASE ivm_install_page")?;
            let reason = bounded_error(&format!("{error:#}"));
            // Do not turn a stale delivery to an already terminal job into another transition.
            mark_stopped(tx, id, &reason)?;
            Ok(Outcome::Stopped(reason))
        }
    }
}
fn load_job(connection: &Connection, id: &str) -> Result<Job> {
    let row = connection.query_row("SELECT view,fingerprint,source,source_fingerprint,epoch,base_revision,applied_revision,cursor,phase,error,
        queued_rows,queued_bytes,limits,started_ms,pages,extracted_rows,applied_rows,max_page_us FROM ivm_install_jobs WHERE id=?1",[id], |r| {
        Ok((Progress { id:id.into(),view:r.get(0)?,base_revision:r.get(5)?,applied_revision:r.get(6)?,cursor:r.get(7)?,phase:r.get(8)?,error:r.get(9)?,
            queued_rows:r.get(10)?,queued_bytes:r.get(11)?,pages:r.get(14)?,extracted_rows:r.get(15)?,applied_rows:r.get(16)?,max_page_us:r.get(17)? },
            r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,u64>(4)?,r.get::<_,String>(12)?,r.get::<_,u64>(13)?))
    }).optional()?.context("unknown installation job")?;
    Ok(Job {
        progress: row.0,
        fingerprint: row.1,
        source: row.2,
        source_fingerprint: row.3,
        epoch: row.4,
        limits: serde_json::from_str(&row.5)?,
        started_ms: row.6,
    })
}
