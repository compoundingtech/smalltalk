//! Local-only evidence for the experimental shared-envelope size check.
//!
//! Every marker and counter change is in the append/sealing transaction. Reads use one
//! bounded point lookup; a pending marker is uncertified, never a successful size check.
use std::fmt;

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SUMMARY: &str = "shared_append/status-v1";
const MAX_VALUE: usize = 8192;
const MAX_DETAILS: usize = 4;
const VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Marker {
    version: u32,
    pub(crate) incarnation: String,
    pub(crate) batch_digest: String,
    encoding_policy: String,
    source: Option<String>,
    pub(crate) limit: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Fault {
    pub code: String,
    pub batch_digest: String,
    pub incarnation: String,
    pub actual_bytes: u64,
    pub limit_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Summary {
    version: u32,
    pub pending: u64,
    pub faults: u64,
    pub details: Vec<Fault>,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            version: VERSION,
            pending: 0,
            faults: 0,
            details: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    /// None means uncomputed, including stores that have never enabled this experiment.
    pub durable: Option<Summary>,
    /// Additional process-local detail, deduplicated against the retained summary. The
    /// whole report contains at most four details, including this observation.
    pub observed: Option<Fault>,
    pub publication_failed: bool,
}

impl Report {
    pub fn has_faults(&self) -> bool {
        self.publication_failed
            || self.observed.is_some()
            || self.durable.as_ref().is_some_and(|s| s.faults > 0)
    }

    pub fn verified(&self) -> bool {
        !self.publication_failed
            && self.observed.is_none()
            && self
                .durable
                .as_ref()
                .is_some_and(|s| s.pending == 0 && s.faults == 0)
    }

    pub fn more_details(&self) -> bool {
        self.durable
            .as_ref()
            .is_some_and(|s| s.faults > s.details.len() as u64)
    }
}

#[derive(Clone, Default)]
pub(crate) struct Transient {
    pub(crate) observed: Option<Fault>,
    pub(crate) publication_failed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Oversize {
    pub(crate) marker: Marker,
    pub(crate) fault: Fault,
}

impl fmt::Display for Oversize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shared-append-payload-oversize batch={} bytes={} limit={}",
            self.fault.batch_digest, self.fault.actual_bytes, self.fault.limit_bytes
        )
    }
}
impl std::error::Error for Oversize {}

#[derive(Debug)]
pub struct SealingFailure {
    pub report: Report,
}

impl fmt::Display for SealingFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "shared-append-sealing-fault pending={} faults={} observed={} publication_failed={}",
            self.report.durable.as_ref().map_or(0, |s| s.pending),
            self.report.durable.as_ref().map_or(0, |s| s.faults),
            self.report.observed.is_some(),
            self.report.publication_failed
        )
    }
}
impl std::error::Error for SealingFailure {}

fn digest(batch: &str) -> String {
    hex::encode(Sha256::digest(batch.as_bytes()))
}
fn marker_key(digest: &str) -> String {
    format!("shared_append/mark/{digest}")
}
fn fault_key(digest: &str) -> String {
    format!("shared_append/fault/{digest}")
}

fn read<T: DeserializeOwned>(connection: &Connection, key: &str) -> Result<Option<T>> {
    // Do not materialize arbitrary-sized malformed metadata in the read accessor.
    let text: Option<Option<String>> = connection.prepare_cached(
        "SELECT CASE WHEN length(CAST(value AS BLOB))<=?2 THEN value END FROM meta WHERE key=?1"
    )?.query_row(params![key, MAX_VALUE], |row| row.get(0)).optional()?;
    match text {
        None => Ok(None),
        Some(Some(text)) => Ok(Some(serde_json::from_str(&text)?)),
        Some(None) => anyhow::bail!("oversized shared append metadata"),
    }
}

fn write<T: Serialize>(tx: &Transaction<'_>, key: &str, value: &T) -> Result<()> {
    let value = serde_json::to_string(value)?;
    ensure!(value.len() <= MAX_VALUE, "oversized shared append metadata");
    tx.prepare_cached("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value")?
        .execute(params![key, value])?;
    Ok(())
}

pub(crate) fn summary(connection: &Connection) -> Result<Option<Summary>> {
    let summary = read::<Summary>(connection, SUMMARY)?;
    if let Some(s) = &summary {
        ensure!(
            s.version == VERSION
                && s.details.len() <= MAX_DETAILS
                && s.details.len() as u64 <= s.faults
                && s.faults <= s.pending,
            "invalid shared append summary"
        );
        for detail in &s.details {
            validate_fault(detail)?;
        }
    }
    Ok(summary)
}

pub(crate) fn report(connection: &Connection, transient: &Transient) -> Result<Report> {
    let mut durable = summary(connection)?;
    let observed = transient.observed.clone().filter(|observation| {
        !durable
            .as_ref()
            .is_some_and(|summary| summary.details.contains(observation))
    });
    if observed.is_some()
        && let Some(summary) = &mut durable
    {
        summary.details.truncate(MAX_DETAILS - 1);
    }
    Ok(Report {
        durable,
        observed,
        publication_failed: transient.publication_failed,
    })
}

fn validate_marker(marker: &Marker) -> Result<()> {
    ensure!(
        marker.version == VERSION
            && marker.encoding_policy == "replica-payload-cbor-v1"
            && marker.incarnation.len() == 32
            && marker.batch_digest.len() == 64
            && marker
                .source
                .as_ref()
                .is_none_or(|source| source.len() <= 128)
            && marker.limit == crate::append_group::REPLICA_BATCH_BYTES as u64,
        "invalid shared append marker"
    );
    Ok(())
}

fn validate_fault(fault: &Fault) -> Result<()> {
    ensure!(
        fault.code == "shared-append-payload-oversize"
            && fault.batch_digest.len() == 64
            && fault.incarnation.len() == 32
            && fault.actual_bytes > fault.limit_bytes
            && fault.limit_bytes == crate::append_group::REPLICA_BATCH_BYTES as u64,
        "invalid shared append fault"
    );
    Ok(())
}

/// Returns the number of intended meta row mutations for the frontier's epoch guard.
pub(crate) fn mark(tx: &Transaction<'_>, batch: &str) -> Result<u64> {
    let batch_digest = digest(batch);
    let marker = Marker {
        version: VERSION,
        incarnation: Uuid::now_v7().simple().to_string(),
        batch_digest: batch_digest.clone(),
        encoding_policy: "replica-payload-cbor-v1".into(),
        source: option_env!("ST3_BUILD_REVISION").map(str::to_owned),
        limit: crate::append_group::REPLICA_BATCH_BYTES as u64,
    };
    validate_marker(&marker)?;
    let mut s = summary(tx)?.unwrap_or_default();
    s.pending = s
        .pending
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("shared append pending overflow"))?;
    tx.prepare_cached("INSERT INTO meta(key,value) VALUES(?1,?2)")?
        .execute(params![
            marker_key(&batch_digest),
            serde_json::to_string(&marker)?
        ])?;
    write(tx, SUMMARY, &s)?;
    Ok(2)
}

/// Checks actual complete serialized payload bytes, including every stored/new signature.
/// Existing serial/forced history has no marker and is never retroactively capped.
pub(crate) fn check(tx: &Transaction<'_>, batch: &str, bytes: usize) -> Result<Option<Marker>> {
    let digest = digest(batch);
    let Some(marker) = read::<Marker>(tx, &marker_key(&digest))? else {
        return Ok(None);
    };
    validate_marker(&marker)?;
    ensure!(
        marker.batch_digest == digest,
        "shared append marker identity mismatch"
    );
    if bytes as u64 > marker.limit {
        let fault = Fault {
            code: "shared-append-payload-oversize".into(),
            batch_digest: digest,
            incarnation: marker.incarnation.clone(),
            actual_bytes: bytes as u64,
            limit_bytes: marker.limit,
        };
        return Err(Oversize { marker, fault }.into());
    }
    Ok(Some(marker))
}

/// Called in a separate evidence transaction only after the complete sealing chunk rolled back,
/// while the same writer loan still excludes racing publishers/clears.
pub(crate) fn persist(tx: &Transaction<'_>, observed: &Oversize) -> Result<()> {
    let current = read::<Marker>(tx, &marker_key(&observed.marker.batch_digest))?;
    ensure!(
        current.as_ref() == Some(&observed.marker),
        "shared append marker incarnation changed"
    );
    let key = fault_key(&observed.marker.batch_digest);
    if let Some(existing) = read::<Fault>(tx, &key)? {
        ensure!(
            existing == observed.fault,
            "shared append fault identity changed"
        );
        return Ok(());
    }
    let mut s = summary(tx)?.ok_or_else(|| anyhow::anyhow!("shared append summary missing"))?;
    s.faults = s
        .faults
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("shared append fault overflow"))?;
    ensure!(
        s.faults <= s.pending,
        "shared append fault count exceeds pending"
    );
    if s.details.len() < MAX_DETAILS {
        s.details.push(observed.fault.clone());
    }
    write(tx, &key, &observed.fault)?;
    write(tx, SUMMARY, &s)
}

/// Resolve only the exact marker whose payload was successfully checked in this transaction.
/// Its deletion and envelope/signature persistence become durable in the same real COMMIT.
pub(crate) fn resolved(tx: &Transaction<'_>, marker: &Marker) -> Result<()> {
    let current = read::<Marker>(tx, &marker_key(&marker.batch_digest))?;
    ensure!(
        current.as_ref() == Some(marker),
        "shared append marker incarnation changed"
    );
    let mut s = summary(tx)?.ok_or_else(|| anyhow::anyhow!("shared append summary missing"))?;
    let key = fault_key(&marker.batch_digest);
    if let Some(fault) = read::<Fault>(tx, &key)? {
        ensure!(
            fault.incarnation == marker.incarnation,
            "shared append fault incarnation changed"
        );
        ensure!(
            tx.execute("DELETE FROM meta WHERE key=?1", [&key])? == 1,
            "shared append fault disappeared"
        );
        s.faults = s
            .faults
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("shared append fault underflow"))?;
        s.details.retain(|f| {
            f.batch_digest != marker.batch_digest || f.incarnation != marker.incarnation
        });
    }
    ensure!(
        tx.execute(
            "DELETE FROM meta WHERE key=?1",
            [marker_key(&marker.batch_digest)]
        )? == 1,
        "shared append marker disappeared"
    );
    s.pending = s
        .pending
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("shared append pending underflow"))?;
    write(tx, SUMMARY, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) WITHOUT ROWID;",
        )
        .unwrap();
        conn
    }

    fn mark_commit(conn: &mut Connection, batch: &str) {
        let tx = conn.transaction().unwrap();
        mark(&tx, batch).unwrap();
        tx.commit().unwrap();
    }

    fn observe(conn: &mut Connection, batch: &str) -> Oversize {
        let tx = conn.transaction().unwrap();
        let error = check(&tx, batch, crate::append_group::REPLICA_BATCH_BYTES + 1).unwrap_err();
        error.downcast::<Oversize>().unwrap()
    }

    #[test]
    fn pending_is_uncertified_and_marker_rollback_is_atomic() {
        let mut conn = connection();
        assert!(!report(&conn, &Transient::default()).unwrap().verified());
        {
            let tx = conn.transaction().unwrap();
            mark(&tx, "batch/sample").unwrap();
        }
        assert!(summary(&conn).unwrap().is_none());
        mark_commit(&mut conn, "batch/sample");
        let r = report(&conn, &Transient::default()).unwrap();
        assert!(!r.verified());
        assert_eq!(r.durable.unwrap().pending, 1);
    }

    #[test]
    fn exact_boundary_and_unmarked_legacy_are_distinct() {
        let mut conn = connection();
        mark_commit(&mut conn, "batch/sample");
        let tx = conn.transaction().unwrap();
        assert!(
            check(
                &tx,
                "batch/sample",
                crate::append_group::REPLICA_BATCH_BYTES
            )
            .unwrap()
            .is_some()
        );
        assert!(
            check(
                &tx,
                "batch/sample",
                crate::append_group::REPLICA_BATCH_BYTES + 1
            )
            .is_err()
        );
        assert!(check(&tx, "batch/legacy", usize::MAX).unwrap().is_none());
    }

    #[test]
    fn evidence_commit_failure_keeps_pending_and_transient_fault() {
        let mut conn = connection();
        mark_commit(&mut conn, "batch/sample");
        let observed = observe(&mut conn, "batch/sample");
        {
            let tx = conn.transaction().unwrap();
            persist(&tx, &observed).unwrap();
        }
        let s = summary(&conn).unwrap().unwrap();
        assert_eq!(s.pending, 1);
        assert_eq!(s.faults, 0);
        let transient = Transient {
            observed: Some(observed.fault),
            publication_failed: true,
        };
        assert!(report(&conn, &transient).unwrap().has_faults());
        assert!(!report(&conn, &transient).unwrap().verified());
        assert!(!report(&conn, &Transient::default()).unwrap().verified());
    }

    #[test]
    fn many_faults_have_bounded_point_read_and_duplicate_publication_is_idempotent() {
        let mut conn = connection();
        for n in 0..9 {
            let batch = format!("batch/{n}");
            mark_commit(&mut conn, &batch);
            let observed = observe(&mut conn, &batch);
            let tx = conn.transaction().unwrap();
            persist(&tx, &observed).unwrap();
            persist(&tx, &observed).unwrap();
            tx.commit().unwrap();
        }
        let r = report(&conn, &Transient::default()).unwrap();
        assert!(r.has_faults());
        assert!(r.more_details());
        let s = r.durable.unwrap();
        assert_eq!(s.faults, 9);
        assert_eq!(s.details.len(), 4);
        assert!(serde_json::to_string(&s).unwrap().len() < MAX_VALUE);
        let other = observe(&mut conn, "batch/8");
        let transient = Transient {
            observed: Some(other.fault),
            publication_failed: false,
        };
        let report = report(&conn, &transient).unwrap();
        assert_eq!(
            report.durable.as_ref().unwrap().details.len() + usize::from(report.observed.is_some()),
            4
        );
        assert!(report.more_details());
    }

    #[test]
    fn a_successful_unrelated_batch_never_clears_a_known_fault() {
        let mut conn = connection();
        mark_commit(&mut conn, "batch/bad");
        mark_commit(&mut conn, "batch/good");
        let observed = observe(&mut conn, "batch/bad");
        let tx = conn.transaction().unwrap();
        persist(&tx, &observed).unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        let marker = check(&tx, "batch/good", 1).unwrap().unwrap();
        resolved(&tx, &marker).unwrap();
        tx.commit().unwrap();
        let s = summary(&conn).unwrap().unwrap();
        assert_eq!((s.pending, s.faults), (1, 1));
    }

    #[test]
    fn stale_incarnation_cannot_publish_or_clear() {
        let mut conn = connection();
        mark_commit(&mut conn, "batch/sample");
        let observed = observe(&mut conn, "batch/sample");
        let tx = conn.transaction().unwrap();
        let mut newer = observed.marker.clone();
        newer.incarnation = Uuid::now_v7().simple().to_string();
        write(&tx, &marker_key(&newer.batch_digest), &newer).unwrap();
        tx.commit().unwrap();
        let tx = conn.transaction().unwrap();
        assert!(persist(&tx, &observed).is_err());
        assert!(resolved(&tx, &observed.marker).is_err());
    }

    #[test]
    fn missing_batch_does_not_clear_and_malformed_summary_is_an_error() {
        let mut conn = connection();
        mark_commit(&mut conn, "batch/missing");
        assert!(!report(&conn, &Transient::default()).unwrap().verified());
        conn.execute(
            "UPDATE meta SET value=?1 WHERE key=?2",
            params!["x".repeat(MAX_VALUE + 1), SUMMARY],
        )
        .unwrap();
        assert!(report(&conn, &Transient::default()).is_err());
    }
}
