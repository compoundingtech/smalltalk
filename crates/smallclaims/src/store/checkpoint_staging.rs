//! A checkpoint's sealed set and plan, staged in a file on disk, so a checkpoint pass holds one
//! page of the set at a time however large the store is (#2492). See
//! `docs/st3/checkpoint-capture.md`.
//!
//! The capture is pinned to a store-index cut: the seal row, the cut, and the value of the
//! persistent `checkpoint_capture_epoch`, whose triggers move it on every change to a row the
//! set could include. A staging file whose epoch still matches holds exactly the set the store
//! holds, however long ago it was started and however many restarts came between, so a capture
//! resumes at its cursor instead of starting again. Each page is read from the store on a
//! snapshot of its own, after checking the epoch, and written to the staging file with its
//! cursor in one transaction, so a crash leaves a consistent prefix.
//!
//! The tables are the planner's contract: a runtime's `plan_staged_checkpoint_drops` reads
//! `claims` and marks `dropped`, and `finish` derives the plan from those marks.
use super::checkpoint::{
    CheckpointCaptureChanged, ClaimTombstone, DropCount, EnvelopeKey, EnvelopeTombstone, ProofInput,
    RECORDED_TOMBSTONE_FILTER, checkpoint_accepted_time, checkpoint_name, digest_field,
    sealed_records_page_sql,
};
use super::*;

/// Bump on any change to the tables below or to what a phase writes, so a newer build never
/// resumes a file an older one staged.
const STAGING_FORMAT: i64 = 1;
/// Rows each page reads from the store.
pub const STAGING_PAGE: i64 = 64;
/// SQLite's page cache for the staging file, in KiB. Its sorter spills past it to disk.
const STAGING_CACHE_KIB: i64 = 2_048;

const SCHEMA: &str = /* sqlite-large-value: allow-for-now lv-hb1622b55b32a owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-variable-columns */ "
CREATE TABLE IF NOT EXISTS meta(
    id INTEGER PRIMARY KEY CHECK(id=1),
    identity TEXT NOT NULL,
    phase INTEGER NOT NULL,
    cursor INTEGER NOT NULL,
    bound INTEGER,
    plan TEXT
);
CREATE TABLE IF NOT EXISTS envelopes(
    writer TEXT NOT NULL, sequence INTEGER NOT NULL, envelope_hash TEXT NOT NULL,
    accepted_at INTEGER NOT NULL, records INTEGER NOT NULL, included INTEGER NOT NULL,
    PRIMARY KEY(writer, sequence, envelope_hash)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS records(
    k0 INTEGER, k1 TEXT, k2 TEXT, k3 INTEGER, k4 TEXT, k5 INTEGER, claim_id TEXT NOT NULL,
    writer TEXT NOT NULL, sequence INTEGER NOT NULL, envelope_hash TEXT NOT NULL,
    position INTEGER, valid INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS ordered(n INTEGER PRIMARY KEY, record INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS claims(
    pos INTEGER PRIMARY KEY, id TEXT NOT NULL,
    writer TEXT NOT NULL, sequence INTEGER NOT NULL, envelope_hash TEXT NOT NULL,
    subject TEXT NOT NULL, kind TEXT NOT NULL, actor TEXT, operation_id TEXT,
    request_digest TEXT, predecessors TEXT NOT NULL, accepted_at INTEGER NOT NULL,
    valid INTEGER NOT NULL, first INTEGER NOT NULL, protected INTEGER NOT NULL DEFAULT 0,
    slot TEXT, seat TEXT, fields TEXT NOT NULL, limits_group TEXT, limits_measured INTEGER,
    status_keep INTEGER NOT NULL DEFAULT 0, dropped INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS claims_id ON claims(id);
CREATE INDEX IF NOT EXISTS claims_slot ON claims(slot, pos) WHERE slot IS NOT NULL;
CREATE INDEX IF NOT EXISTS claims_seat ON claims(seat, pos) WHERE seat IS NOT NULL;
CREATE INDEX IF NOT EXISTS claims_operation ON claims(operation_id) WHERE operation_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS claims_envelope ON claims(writer, sequence, envelope_hash);
CREATE TABLE IF NOT EXISTS cited(id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS closed(subject TEXT NOT NULL, request TEXT NOT NULL,
    PRIMARY KEY(subject, request)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS protected(id TEXT PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS earlier_envelopes(
    writer TEXT NOT NULL, sequence INTEGER NOT NULL, envelope_hash TEXT NOT NULL,
    accepted_at INTEGER NOT NULL, PRIMARY KEY(writer, sequence, envelope_hash)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS earlier_claims(
    id TEXT PRIMARY KEY, writer TEXT NOT NULL, sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL, actor TEXT,
    predecessors TEXT NOT NULL, operation_id TEXT, request_digest TEXT,
    accepted_at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS dropped_envelopes(
    n INTEGER PRIMARY KEY, writer TEXT NOT NULL, sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL, accepted_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS dropped_claims(
    n INTEGER PRIMARY KEY, id TEXT NOT NULL, writer TEXT NOT NULL, sequence INTEGER NOT NULL,
    envelope_hash TEXT NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL, actor TEXT,
    predecessors TEXT NOT NULL, operation_id TEXT, request_digest TEXT,
    accepted_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS dropped_claims_subject ON dropped_claims(subject);
";

/// The capture's phases, in order. Each but `ORDER` reads one table of the store a page at a
/// time; the cursor is the last row read.
const ENVELOPES: i64 = 0;
const RECORDS: i64 = 1;
const ORDER: i64 = 2;
const CLAIMS: i64 = 3;
/// One phase per protection query, `PROTECTED..PROTECTED + PROTECTIONS.len()`.
const PROTECTED: i64 = 4;
const PROTECTIONS: [(&str, &str, &str); 6] = [
    ("desired", "claim_id", "claim_id IS NOT NULL AND ?3 IS NOT NULL"),
    ("mission_definitions", "claim_id", "claim_id IS NOT NULL AND ?3 IS NOT NULL"),
    ("mission_revisions", "claim_id", "claim_id IS NOT NULL AND ?3 IS NOT NULL"),
    ("documents", "binding_claim_id", "binding_claim_id IS NOT NULL AND ?3 IS NOT NULL"),
    ("claims", "json_extract(body, '$.fields.replacement')", "kind='record.repaired' AND ?3 IS NOT NULL"),
    ("replica_records", "replacement_claim_id", "replacement_claim_id IS NOT NULL AND ?3 IS NOT NULL"),
];
const EARLIER_ENVELOPES: i64 = PROTECTED + PROTECTIONS.len() as i64;
const EARLIER_CLAIMS: i64 = EARLIER_ENVELOPES + 1;
const CAPTURED: i64 = EARLIER_CLAIMS + 1;

/// What a planner reads of one sealed claim's body. The capture computes it once, while the
/// body is in hand, so the plan holds no bodies.
#[derive(Clone, Debug, Default)]
pub struct CheckpointClaimFacts {
    /// The claim's rule and slot, encoded by the runtime, when a rule may drop it.
    pub slot: Option<String>,
    /// Whether the claim is a source of its subject's status history.
    pub status_source: bool,
    /// The names of the fields it sets.
    pub fields: Vec<String>,
    /// The claim IDs it cites as evidence or pins as an input.
    pub cites: Vec<String>,
    /// The request it closes on its subject.
    pub closes: Option<String>,
    /// Its limits account group, encoded by the runtime, and when it was measured.
    pub limits: Option<(String, u64)>,
}

/// The counts and digests of a staged plan. Its tombstones stay in the staging file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedPlan {
    pub cut_unix_ms: u128,
    pub rules_digest: String,
    pub sealed_envelopes: usize,
    pub sealed_claims: usize,
    pub sealed_digest: String,
    pub dropped_envelopes: usize,
    pub dropped_claims: usize,
    pub by_kind: BTreeMap<String, DropCount>,
    pub drop_digest: String,
    pub retained_digest: String,
}

impl StagedPlan {
    /// The counts and digests of a plan made in memory.
    pub fn of(plan: &super::checkpoint::DropPlan) -> Self {
        Self {
            cut_unix_ms: plan.cut_unix_ms,
            rules_digest: plan.rules_digest.clone(),
            sealed_envelopes: plan.sealed_envelopes,
            sealed_claims: plan.sealed_claims,
            sealed_digest: plan.sealed_digest.clone(),
            dropped_envelopes: plan.envelopes.len(),
            dropped_claims: plan.claims.len(),
            by_kind: plan.by_kind.clone(),
            drop_digest: plan.drop_digest.clone(),
            retained_digest: plan.retained_digest.clone(),
        }
    }
}

/// A sealed set captured to a staging file, and its plan once made.
pub struct StagedSet {
    pub path: PathBuf,
    staging: Connection,
    /// Held for the life of the set: one capture per directory at a time.
    _lock: fs::File,
    pub cut_unix_ms: u128,
    pub seal_rowid: i64,
    epoch: i64,
}

/// The name a staging file has for one checkpoint and seal row.
fn staging_name(cut_unix_ms: u128, seal_rowid: i64) -> String {
    format!("capture-{}-{seal_rowid}.sqlite3", checkpoint_name(cut_unix_ms).replace('/', "-"))
}

/// Delete every staging file in `dir` but `keep`. A directory holds one capture at a time, so a
/// file for another checkpoint or seal row is one this node no longer needs.
fn remove_other_staging(dir: &Path, keep: Option<&Path>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if !name.starts_with("capture-") {
            continue;
        }
        let base = keep.map(|keep| keep.as_os_str().to_string_lossy().into_owned());
        let own = base.as_deref().is_some_and(|base| {
            let path = path.to_string_lossy();
            path == base || path.strip_prefix(base).is_some_and(|suffix| suffix.starts_with('-'))
        });
        if !own {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Take the capture lock of `dir`, or fail at once when another capture holds it. The loser's
/// pass fails, and its next pass tries again; it never waits behind the other capture.
fn lock_staging(dir: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd as _;
    fs::create_dir_all(dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("capture.lock"))?;
    // SAFETY: the descriptor is open for the length of the call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::bail!("another checkpoint capture is running in {}", dir.display());
        }
        return Err(error.into());
    }
    Ok(lock)
}

fn accepted(value: u128) -> Result<i64> {
    Ok(i64::try_from(value)?)
}

fn u128_of(value: i64) -> u128 {
    u128::try_from(value).unwrap_or(0)
}

impl Store {
    /// Capture the sealed set before `cut_unix_ms`, as of `through_rowid`, into a staging file in
    /// `dir`, resuming a capture of the same store-index cut that a previous pass or process left
    /// there. See `checkpoint_sealed_set_paged` for the epoch: three conflicting changes to the
    /// captured history, each discarding the file, fail closed.
    pub fn checkpoint_staged_set(
        &self,
        cut_unix_ms: u128,
        through_rowid: Option<i64>,
        dir: &Path,
    ) -> Result<StagedSet> {
        self.runtime.checkpoint_preflight()?;
        let lock = lock_staging(dir)?;
        let seal_rowid = self.register_checkpoint_capture(cut_unix_ms, through_rowid)?;
        let path = dir.join(staging_name(cut_unix_ms, seal_rowid));
        remove_other_staging(dir, Some(&path))?;
        let mut lock = Some(lock);
        for _ in 0..3 {
            let epoch: i64 = self.read_snapshot(|_| {
                Ok(self.readers.get().query_row(
                    "SELECT value FROM checkpoint_capture_epoch WHERE id=1", [], |row| row.get(0),
                )?)
            })?;
            let staged = StagedSet::open(
                &path,
                lock.take().expect("each attempt takes the lock back"),
                self.staging_identity(cut_unix_ms, seal_rowid, epoch)?,
                cut_unix_ms,
                seal_rowid,
                epoch,
            )?;
            match staged.capture(self) {
                Err(error) if error.downcast_ref::<CheckpointCaptureChanged>().is_some() => {
                    lock = Some(staged.discard()?);
                }
                Err(error) => return Err(error),
                Ok(()) => return Ok(staged),
            }
        }
        anyhow::bail!("checkpoint capture invalidated by concurrent store changes after 3 attempts")
    }

    /// Delete the staging files in `dir`, as when this node has no checkpoint left to capture.
    /// A capture holding the directory keeps its file.
    pub fn remove_checkpoint_staging(&self, dir: &Path) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        match lock_staging(dir) {
            Ok(_lock) => remove_other_staging(dir, None),
            Err(_) => Ok(()),
        }
    }

    /// What a staging file must match to be resumed: its format, cut, seal row and epoch, the
    /// rules, and this executable, since the runtime computes each claim's facts and a build
    /// that changes how is a different executable.
    fn staging_identity(&self, cut_unix_ms: u128, seal_rowid: i64, epoch: i64) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"smallclaims-checkpoint-staging-v1\0");
        digest.update(STAGING_FORMAT.to_be_bytes());
        digest.update(cut_unix_ms.to_be_bytes());
        digest.update(seal_rowid.to_be_bytes());
        digest.update(epoch.to_be_bytes());
        for field in [
            self.runtime.checkpoint_rules_digest(),
            super::checkpoint_agreement::checkpoint_build(),
            super::checkpoint_agreement::checkpoint_executable_digest()?.to_owned(),
        ] {
            digest_field(&mut digest, Some(&field));
        }
        Ok(hex::encode(digest.finalize()))
    }

    /// Plan the staged set, or return the plan it already holds: a plan is a pure function of
    /// the set and the rules, and the file's identity fixes both.
    pub fn plan_staged_checkpoint(&self, staged: &StagedSet) -> Result<StagedPlan> {
        self.runtime.checkpoint_preflight()?;
        if let Some(plan) = staged.plan()? {
            return Ok(plan);
        }
        // One transaction on the staging file: a plan cut short leaves no marks behind.
        let transaction = staged.staging.unchecked_transaction()?;
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-hb78dff66c1f1 owner=smallclaims::store::checkpoint_staging TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET dropped=0, status_keep=0 WHERE dropped<>0 OR status_keep<>0;",
        )?;
        self.runtime.plan_staged_checkpoint_drops(&StagedPlanner { store: self, staged })?;
        let plan = staged.finish(&transaction, self.runtime.checkpoint_rules_digest())?;
        transaction.commit()?;
        #[cfg(any(test, feature = "test-support"))]
        if REFERENCE_PLAN.with(std::cell::Cell::get) {
            self.check_against_reference_plan(staged, &plan)?;
        }
        Ok(plan)
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static REFERENCE_PLAN: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Run `work` without comparing each staged plan with the in-memory reference planner, as a
/// test of the staged path's memory must: the reference holds the whole set.
#[cfg(any(test, feature = "test-support"))]
pub fn without_reference_plan<T>(work: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            REFERENCE_PLAN.with(|reference| reference.set(self.0));
        }
    }
    let _restore = Restore(REFERENCE_PLAN.with(|reference| reference.replace(false)));
    work()
}

#[cfg(any(test, feature = "test-support"))]
impl Store {
    /// In test builds every staged plan is also made in memory, by the reference planner, from
    /// the same seal row, and must be the same plan, tombstone for tombstone.
    fn check_against_reference_plan(&self, staged: &StagedSet, plan: &StagedPlan) -> Result<()> {
        let sealed = self.checkpoint_sealed_set_through(staged.cut_unix_ms, Some(staged.seal_rowid))?;
        let reference = self.runtime.plan_checkpoint_drops(&sealed);
        assert_eq!(&StagedPlan::of(&reference), plan, "the staged plan differs from the in-memory plan");
        assert_eq!(
            staged.dropped_envelopes(0, usize::MAX >> 1)?,
            reference.envelopes,
            "the staged plan drops other envelopes"
        );
        let mut claims = reference.claims;
        claims.sort();
        assert_eq!(staged.dropped_claims(0, usize::MAX >> 1)?, claims, "the staged plan drops other claims");
        Ok(())
    }
}

/// What a runtime's staged planner reads and marks.
pub struct StagedPlanner<'a> {
    store: &'a Store,
    staged: &'a StagedSet,
}

impl StagedPlanner<'_> {
    /// The staging file. `claims` holds one row per sealed record in canonical order (`pos`),
    /// with the runtime's facts; the planner sets `status_keep` and `dropped`.
    pub fn staging(&self) -> &Connection {
        &self.staged.staging
    }

    pub fn cut_unix_ms(&self) -> u128 {
        self.staged.cut_unix_ms
    }

    /// The claims with these IDs, in this order, read from the store a page at a time. The
    /// epoch check makes them the captured claims.
    pub fn claims(&self, ids: &[String]) -> Result<Vec<ClaimRecord>> {
        let sql = format!("SELECT {CLAIM_COLUMNS} FROM claims WHERE id=?1");
        let mut claims = Vec::with_capacity(ids.len());
        for page in ids.chunks(STAGING_PAGE as usize) {
            claims.extend(self.store.checkpoint_capture_page(self.staged.epoch, || {
                let connection = self.store.readers.get();
                let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-h319550c93fab owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?;
                page.iter()
                    .map(|id| Ok(statement.query_row(/* sqlite-large-value: allow-for-now lv-h972ac6abbb64 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [id], claim_from_row)?))
                    .collect::<Result<Vec<_>>>()
            })?);
        }
        Ok(claims)
    }
}

impl StagedSet {
    fn open(
        path: &Path,
        lock: fs::File,
        identity: String,
        cut_unix_ms: u128,
        seal_rowid: i64,
        epoch: i64,
    ) -> Result<Self> {
        let connect = || -> Result<Connection> {
            let staging = Connection::open(path)?;
            // The file is a cache of the store: a crash may lose the last page, never corrupt
            // what was committed, and a lost file is captured again.
            staging.execute_batch(/* sqlite-large-value: allow-for-now lv-h2b1120fcc098 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!(
                "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;
                 PRAGMA cache_size = -{STAGING_CACHE_KIB}; PRAGMA temp_store = FILE;"
            ))?;
            staging.execute_batch(/* sqlite-large-value: allow-for-now lv-h53697c1c92c4 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ SCHEMA)?;
            Ok(staging)
        };
        let mut staging = connect()?;
        let current: Option<String> = staging
            .query_row("SELECT identity FROM meta WHERE id=1", [], |row| row.get(0))
            .optional()?;
        if current.as_deref() != Some(identity.as_str()) {
            drop(staging);
            remove_file_set(path)?;
            staging = connect()?;
            staging.execute(
                /* sqlite-large-value: allow-for-now lv-h20bffcbd0763 owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO meta(id, identity, phase, cursor) VALUES (1, ?1, ?2, 0)",
                params![identity, ENVELOPES],
            )?;
        }
        Ok(Self {
            path: path.to_owned(),
            staging,
            _lock: lock,
            cut_unix_ms,
            seal_rowid,
            epoch,
        })
    }

    /// Delete the staging file and give back the lock, for a capture that starts again.
    fn discard(self) -> Result<fs::File> {
        let Self { path, staging, _lock, .. } = self;
        drop(staging);
        remove_file_set(&path)?;
        Ok(_lock)
    }

    /// Delete the staging file, as when its checkpoint is trimmed.
    pub fn remove(self) -> Result<()> {
        self.discard().map(drop)
    }

    fn position(&self) -> Result<(i64, i64, Option<i64>)> {
        Ok(self.staging.query_row(
            "SELECT phase, cursor, bound FROM meta WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    fn advance(transaction: &Transaction<'_>, phase: i64, cursor: i64, bound: Option<i64>) -> Result<()> {
        transaction.execute(
            /* sqlite-large-value: allow-for-now lv-ha4524d62a122 owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "UPDATE meta SET phase=?1, cursor=?2, bound=?3 WHERE id=1",
            params![phase, cursor, bound],
        )?;
        Ok(())
    }

    /// Run the capture's phases from where the file stopped.
    fn capture(&self, store: &Store) -> Result<()> {
        loop {
            let (phase, cursor, bound) = self.position()?;
            match phase {
                ENVELOPES => self.capture_envelopes(store, cursor)?,
                RECORDS => self.capture_records(store, cursor, bound)?,
                ORDER => self.order_records()?,
                CLAIMS => self.capture_claims(store, cursor)?,
                phase if (PROTECTED..EARLIER_ENVELOPES).contains(&phase) => {
                    self.capture_protection(store, phase, cursor, bound)?
                }
                EARLIER_ENVELOPES | EARLIER_CLAIMS => self.capture_earlier(store, phase, cursor, bound)?,
                CAPTURED => {
                    // The last page's epoch check covers every page before it.
                    return store.checkpoint_capture_page(self.epoch, || Ok(()));
                }
                other => anyhow::bail!("checkpoint staging phase {other} is unknown"),
            }
        }
    }

    /// The highest rowid of `table`, read on an epoch-checked page.
    fn high(&self, store: &Store, table: &str) -> Result<i64> {
        store.checkpoint_capture_page(self.epoch, || {
            Ok(store.readers.get().query_row(
                /* sqlite-large-value: allow-for-now lv-h900a449238f9 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!("SELECT COALESCE(MAX(rowid), 0) FROM {table}"), [], |row| row.get(0),
            )?)
        })
    }

    fn capture_envelopes(&self, store: &Store, after: i64) -> Result<()> {
        if after >= self.seal_rowid {
            let transaction = self.staging.unchecked_transaction()?;
            Self::advance(&transaction, RECORDS, 0, None)?;
            return Ok(transaction.commit()?);
        }
        let upto = after.saturating_add(STAGING_PAGE).min(self.seal_rowid);
        let page = store.checkpoint_capture_page(self.epoch, || {
            let connection = store.readers.get();
            let page = connection
                .prepare_cached(
                    "SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                            envelopes.accepted_at_unix_ms,
                            (SELECT COUNT(*) FROM replica_records records
                             WHERE records.writer=envelopes.writer AND records.sequence=envelopes.sequence
                               AND records.envelope_hash=envelopes.envelope_hash)
                     FROM replica_envelopes envelopes
                     WHERE envelopes.rowid > ?1 AND envelopes.rowid <= ?2",
                )?
                .query_map([after, upto], |row| {
                    let accepted: String = row.get(3)?;
                    Ok((
                        EnvelopeKey {
                            writer: row.get(0)?,
                            sequence: row.get(1)?,
                            envelope_hash: row.get(2)?,
                        },
                        accepted.parse::<u128>().map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3, rusqlite::types::Type::Text, Box::new(error),
                            )
                        })?,
                        row.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(page)
        })?;
        let transaction = self.staging.unchecked_transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                /* sqlite-large-value: allow-for-now lv-h0f84e29accc7 owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "INSERT OR REPLACE INTO envelopes VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            )?;
            for (key, accepted_at, records) in page {
                if accepted_at < self.cut_unix_ms {
                    insert.execute(/* sqlite-large-value: allow-for-now lv-hf5ac1a7007a7 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![
                        key.writer, key.sequence, key.envelope_hash, accepted(accepted_at)?, records
                    ])?;
                }
            }
        }
        Self::advance(&transaction, ENVELOPES, upto, None)?;
        Ok(transaction.commit()?)
    }

    /// Records of the staged envelopes, with their canonical order's components. A record of a
    /// claim dated at or after the cut makes its envelope late, and a late envelope is not
    /// before the cut.
    fn capture_records(&self, store: &Store, after: i64, bound: Option<i64>) -> Result<()> {
        let last = match bound {
            Some(last) => last,
            None => self.high(store, "replica_records")?,
        };
        if after >= last {
            let transaction = self.staging.unchecked_transaction()?;
            Self::advance(&transaction, ORDER, 0, None)?;
            return Ok(transaction.commit()?);
        }
        let upto = after.saturating_add(STAGING_PAGE).min(last);
        let cut = accepted(self.cut_unix_ms)?;
        let sql = sealed_records_page_sql();
        type Row = (i64, String, Option<String>, Option<i64>, String, i64, String, String, u64, String, String, i64);
        let page: Vec<Row> = store.checkpoint_capture_page(self.epoch, || {
            let connection = store.readers.get();
            let page = connection
                .prepare_cached(/* sqlite-large-value: allow-for-now lv-h29af95d541e4 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?
                .query_map(params![after, upto, cut, self.seal_rowid], |row| {
                    Ok((
                        row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                        row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?,
                        row.get(10)?, row.get(11)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(page)
        })?;
        let transaction = self.staging.unchecked_transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                /* sqlite-large-value: allow-for-now lv-h33ed5804f30f owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO records VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            let mut late = transaction.prepare_cached(
                /* sqlite-large-value: allow-for-now lv-h13e038eb95a2 owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "UPDATE envelopes SET included=0
                 WHERE writer=?1 AND sequence=?2 AND envelope_hash=?3",
            )?;
            for row in page {
                if checkpoint_accepted_time(&row.1, &row.6, 1)? >= self.cut_unix_ms {
                    late.execute(/* sqlite-large-value: allow-for-now lv-hc2e0a3e9faa7 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![row.7, row.8, row.9])?;
                }
                insert.execute(/* sqlite-large-value: allow-for-now lv-hd395e63608d9 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![
                    row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
                    row.11, row.10 == "valid"
                ])?;
            }
        }
        Self::advance(&transaction, RECORDS, upto, Some(last))?;
        Ok(transaction.commit()?)
    }

    /// Number the records in canonical order, then by the record's own identity, so a claim held
    /// by two records orders the same way every time. SQLite's sorter spills to disk.
    fn order_records(&self) -> Result<()> {
        let transaction = self.staging.unchecked_transaction()?;
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-h7041af4f702e owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "DELETE FROM ordered;
             INSERT INTO ordered(record)
                 SELECT rowid FROM records
                 ORDER BY k0, k1, k2, k3, k4, k5, claim_id, writer, sequence, envelope_hash, position;",
        )?;
        Self::advance(&transaction, CLAIMS, 0, None)?;
        Ok(transaction.commit()?)
    }

    /// The claims of the records of envelopes before the cut, in canonical order, with what the
    /// runtime's planner reads of each body. The bodies go when the page is written.
    fn capture_claims(&self, store: &Store, after: i64) -> Result<()> {
        let page: Vec<(i64, String, EnvelopeKey, bool)> = self
            .staging
            .prepare_cached(
                "SELECT ordered.n, records.claim_id, records.writer, records.sequence,
                        records.envelope_hash, records.valid
                 FROM ordered JOIN records ON records.rowid=ordered.record
                 JOIN envelopes ON envelopes.writer=records.writer
                   AND envelopes.sequence=records.sequence
                   AND envelopes.envelope_hash=records.envelope_hash AND envelopes.included=1
                 WHERE ordered.n > ?1 AND ordered.n <= ?2 ORDER BY ordered.n",
            )?
            .query_map([after, after.saturating_add(STAGING_PAGE)], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    EnvelopeKey {
                        writer: row.get(2)?,
                        sequence: row.get(3)?,
                        envelope_hash: row.get(4)?,
                    },
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let last: i64 = self
            .staging
            .query_row("SELECT COALESCE(MAX(n), 0) FROM ordered", [], |row| row.get(0))?;
        if after >= last {
            let transaction = self.staging.unchecked_transaction()?;
            Self::advance(&transaction, PROTECTED, 0, None)?;
            return Ok(transaction.commit()?);
        }
        let sql = format!("SELECT {CLAIM_COLUMNS} FROM claims WHERE id=?1");
        let claims = store.checkpoint_capture_page(self.epoch, || {
            let connection = store.readers.get();
            let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-hd92c67f5da38 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?;
            page.iter()
                .map(|(_, id, _, _)| Ok(statement.query_row(/* sqlite-large-value: allow-for-now lv-he3939a2c1abd owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [id], claim_from_row)?))
                .collect::<Result<Vec<_>>>()
        })?;
        let transaction = self.staging.unchecked_transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                /* sqlite-large-value: allow-for-now lv-h23eb0955c6c7 owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "INSERT INTO claims(pos, id, writer, sequence, envelope_hash, subject, kind, actor,
                     operation_id, request_digest, predecessors, accepted_at, valid, first, slot,
                     seat, fields, limits_group, limits_measured)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     NOT EXISTS(SELECT 1 FROM claims WHERE id=?2), ?14, ?15, ?16, ?17, ?18)",
            )?;
            let mut first = transaction.prepare_cached("SELECT first FROM claims WHERE pos=?1")?;
            let mut cite = transaction.prepare_cached(/* sqlite-large-value: allow-for-now lv-h43acfbf85a46 owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO cited(id) VALUES (?1)")?;
            let mut close = transaction
                .prepare_cached(/* sqlite-large-value: allow-for-now lv-hf704096aaaec owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO closed(subject, request) VALUES (?1, ?2)")?;
            let mut blob = transaction.prepare_cached(/* sqlite-large-value: allow-for-now lv-hfbccd86acb2d owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO blobs(hash) VALUES (?1)")?;
            for ((pos, _, envelope, valid), claim) in page.into_iter().zip(claims) {
                let facts = store.runtime.checkpoint_claim_facts(&claim);
                let operation = operation_parts(&claim.body);
                insert.execute(/* sqlite-large-value: allow-for-now lv-h1827d5751d61 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![
                    pos,
                    claim.id,
                    envelope.writer,
                    envelope.sequence,
                    envelope.envelope_hash,
                    claim.subject,
                    claim.kind,
                    claim.actor,
                    operation.map(|(id, _)| id),
                    operation.map(|(_, digest)| digest),
                    serde_json::to_string(&claim.predecessors)?,
                    accepted(claim.accepted_at_unix_ms)?,
                    valid,
                    facts.slot,
                    facts.status_source.then_some(&claim.subject),
                    serde_json::to_string(&facts.fields)?,
                    facts.limits.as_ref().map(|(group, _)| group),
                    facts.limits.as_ref().map(|(_, measured)| i64::try_from(*measured)).transpose()?,
                ])?;
                // A rule decides each claim once, by its first occurrence.
                if !first.query_row(/* sqlite-large-value: allow-for-now lv-h3842f574db4a owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [pos], |row| row.get::<_, bool>(0))? {
                    transaction.execute(
                        /* sqlite-large-value: allow-for-now lv-ha83b22db555f owner=smallclaims::store::checkpoint_staging TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET slot=NULL, seat=NULL WHERE pos=?1",
                        [pos],
                    )?;
                }
                for id in &facts.cites {
                    cite.execute(/* sqlite-large-value: allow-for-now lv-h992948526e19 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [id])?;
                }
                if let Some(request) = &facts.closes {
                    close.execute(/* sqlite-large-value: allow-for-now lv-h95f378177293 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![claim.subject, request])?;
                }
                let mut hashes = BTreeSet::new();
                collect_hash_fields(&claim.body, &mut hashes);
                for hash in hashes {
                    blob.execute(/* sqlite-large-value: allow-for-now lv-h7650f366973a owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [hash])?;
                }
            }
        }
        Self::advance(&transaction, CLAIMS, after.saturating_add(STAGING_PAGE).min(last), None)?;
        Ok(transaction.commit()?)
    }

    /// The claims a projection row or a repair names, which stay.
    fn capture_protection(&self, store: &Store, phase: i64, after: i64, bound: Option<i64>) -> Result<()> {
        let (table, columns, filter) = PROTECTIONS[usize::try_from(phase - PROTECTED)?];
        let high = match bound {
            Some(high) => high,
            None => self.high(store, table)?,
        };
        if after >= high {
            let transaction = self.staging.unchecked_transaction()?;
            if phase + 1 == EARLIER_ENVELOPES {
                transaction.execute_batch(
                    /* sqlite-large-value: allow-for-now lv-hdd7330128d54 owner=smallclaims::store::checkpoint_staging TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET protected=1 WHERE id IN (SELECT id FROM protected);",
                )?;
            }
            Self::advance(&transaction, phase + 1, 0, None)?;
            return Ok(transaction.commit()?);
        }
        let upto = after.saturating_add(STAGING_PAGE).min(high);
        let cut = accepted(self.cut_unix_ms)?;
        let sql = format!("SELECT {columns} FROM {table} WHERE rowid>?1 AND rowid<=?2 AND ({filter})");
        let page = store.checkpoint_capture_page(self.epoch, || {
            Ok(store.readers.get().prepare_cached(/* sqlite-large-value: allow-for-now lv-h39c0873480c9 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?
                .query_map(params![after, upto, cut], |row| row.get::<_, Option<String>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let transaction = self.staging.unchecked_transaction()?;
        {
            let mut insert = transaction.prepare_cached(/* sqlite-large-value: allow-for-now lv-hc2f89e8781f9 owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO protected(id) VALUES (?1)")?;
            for id in page.into_iter().flatten() {
                insert.execute(/* sqlite-large-value: allow-for-now lv-h5fd695e5b9b0 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ [id])?;
            }
        }
        Self::advance(&transaction, phase, upto, Some(high))?;
        Ok(transaction.commit()?)
    }

    /// Tombstones dated before the cut, from earlier checkpoints. Their envelopes are part of
    /// what this node seals, with no claims, so a trimmed and an untrimmed node seal the same
    /// identities.
    fn capture_earlier(&self, store: &Store, phase: i64, after: i64, bound: Option<i64>) -> Result<()> {
        let table = if phase == EARLIER_ENVELOPES { "checkpoint_envelopes" } else { "checkpoint_claims" };
        let high = match bound {
            Some(high) => high,
            None => self.high(store, table)?,
        };
        if after >= high {
            let transaction = self.staging.unchecked_transaction()?;
            Self::advance(&transaction, phase + 1, 0, None)?;
            return Ok(transaction.commit()?);
        }
        let upto = after.saturating_add(STAGING_PAGE).min(high);
        let cut = accepted(self.cut_unix_ms)?;
        let transaction;
        if phase == EARLIER_ENVELOPES {
            let sql = format!(
                "SELECT writer, sequence, envelope_hash, accepted_at_unix_ms FROM checkpoint_envelopes
                 WHERE rowid>?1 AND rowid<=?2 AND ({RECORDED_TOMBSTONE_FILTER})"
            );
            let page = store.checkpoint_capture_page(self.epoch, || {
                Ok(store.readers.get().prepare_cached(/* sqlite-large-value: allow-for-now lv-h4431903bf163 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?
                    .query_map(params![after, upto, cut], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            transaction = self.staging.unchecked_transaction()?;
            {
                let mut earlier = transaction.prepare_cached(
                    /* sqlite-large-value: allow-for-now lv-h24b633e89e42 owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "INSERT OR IGNORE INTO earlier_envelopes VALUES (?1, ?2, ?3, ?4)",
                )?;
                // An envelope already before the cut stays as it is; otherwise the tombstone
                // stands for it, with no records.
                let mut envelope = transaction.prepare_cached(
                    /* sqlite-large-value: allow-for-now lv-h0dfb6c4ac55f owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "INSERT INTO envelopes VALUES (?1, ?2, ?3, ?4, 0, 1)
                     ON CONFLICT(writer, sequence, envelope_hash) DO UPDATE
                     SET accepted_at=excluded.accepted_at, records=0, included=1 WHERE included=0",
                )?;
                for (writer, sequence, hash, accepted_at) in page {
                    let accepted_at = accepted_at.max(0);
                    earlier.execute(/* sqlite-large-value: allow-for-now lv-h38f8702967df owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![writer, sequence, hash, accepted_at])?;
                    envelope.execute(/* sqlite-large-value: allow-for-now lv-h902de0c784d8 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![writer, sequence, hash, accepted_at])?;
                }
            }
        } else {
            let sql = format!(
                "SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at_unix_ms FROM checkpoint_claims
                 WHERE rowid>?1 AND rowid<=?2 AND ({RECORDED_TOMBSTONE_FILTER})"
            );
            let page = store.checkpoint_capture_page(self.epoch, || {
                Ok(store.readers.get().prepare_cached(/* sqlite-large-value: allow-for-now lv-h6dbc399d7df7 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &sql)?
                    .query_map(params![after, upto, cut], |row| {
                        let predecessors: Vec<String> =
                            serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default();
                        Ok(ClaimTombstone {
                            id: row.get(0)?,
                            writer: row.get(1)?,
                            sequence: row.get(2)?,
                            envelope_hash: row.get(3)?,
                            subject: row.get(4)?,
                            kind: row.get(5)?,
                            actor: row.get(6)?,
                            predecessors,
                            operation_id: row.get(8)?,
                            request_digest: row.get(9)?,
                            accepted_at_unix_ms: u128::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })?;
            transaction = self.staging.unchecked_transaction()?;
            {
                let mut insert = transaction.prepare_cached(
                    /* sqlite-large-value: allow-for-now lv-h1fca2863ea6d owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO earlier_claims VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )?;
                for tombstone in page {
                    insert.execute(/* sqlite-large-value: allow-for-now lv-h81aa01b77490 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ params![
                        tombstone.id, tombstone.writer, tombstone.sequence, tombstone.envelope_hash,
                        tombstone.subject, tombstone.kind, tombstone.actor,
                        serde_json::to_string(&tombstone.predecessors)?, tombstone.operation_id,
                        tombstone.request_digest, accepted(tombstone.accepted_at_unix_ms)?,
                    ])?;
                }
            }
        }
        Self::advance(&transaction, phase, upto, Some(high))?;
        Ok(transaction.commit()?)
    }

    /// The plan this file holds, if one was made.
    fn plan(&self) -> Result<Option<StagedPlan>> {
        let plan: Option<String> =
            self.staging.query_row("SELECT plan FROM meta WHERE id=1", [], |row| row.get(0))?;
        plan.map(|plan| serde_json::from_str(&plan).map_err(Into::into)).transpose()
    }

    /// Derive the plan from the planner's marks: the new tombstones, the counts and the three
    /// digests, each read from the staging file in the order today's sets sort into, so they are
    /// byte for byte what `plan_drops` gives for the same set.
    fn finish(&self, transaction: &Connection, rules_digest: String) -> Result<StagedPlan> {
        // An envelope goes when every claim it holds goes.
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-h8fa39c3a86bb owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "DELETE FROM dropped_envelopes; DELETE FROM dropped_claims;
             INSERT INTO dropped_envelopes(writer, sequence, envelope_hash, accepted_at)
                 SELECT envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                        envelopes.accepted_at
                 FROM envelopes
                 WHERE envelopes.included=1 AND EXISTS(
                     SELECT 1 FROM claims WHERE claims.writer=envelopes.writer
                       AND claims.sequence=envelopes.sequence
                       AND claims.envelope_hash=envelopes.envelope_hash)
                   AND NOT EXISTS(
                     SELECT 1 FROM claims WHERE claims.writer=envelopes.writer
                       AND claims.sequence=envelopes.sequence
                       AND claims.envelope_hash=envelopes.envelope_hash AND claims.dropped=0)
                 ORDER BY envelopes.writer, envelopes.sequence, envelopes.envelope_hash,
                          envelopes.accepted_at;
             INSERT INTO dropped_claims(id, writer, sequence, envelope_hash, subject, kind, actor,
                     predecessors, operation_id, request_digest, accepted_at)
                 SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at
                 FROM claims WHERE first=1 AND dropped=1 ORDER BY id;",
        )?;
        let count = |sql: &str| -> Result<usize> {
            Ok(usize::try_from(transaction.query_row(/* sqlite-large-value: allow-for-now lv-h4625444c9ccb owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ sql, [], |row| row.get::<_, i64>(0))?)?)
        };
        let sealed_envelopes = count("SELECT COUNT(*) FROM envelopes WHERE included=1")?;
        let sealed_claims = count("SELECT COUNT(*) FROM claims WHERE first=1")?;
        let dropped_envelopes = count("SELECT COUNT(*) FROM dropped_envelopes")?;
        let dropped_claims = count("SELECT COUNT(*) FROM dropped_claims")?;
        let mut by_kind = BTreeMap::new();
        {
            let mut statement = transaction.prepare(
                "SELECT kind, COUNT(*), SUM(dropped) FROM claims WHERE first=1
                 GROUP BY kind HAVING SUM(dropped) > 0",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                by_kind.insert(row.get::<_, String>(0)?, DropCount {
                    sealed: usize::try_from(row.get::<_, i64>(1)?)?,
                    dropped: usize::try_from(row.get::<_, i64>(2)?)?,
                });
            }
        }
        let plan = StagedPlan {
            cut_unix_ms: self.cut_unix_ms,
            rules_digest,
            sealed_envelopes,
            sealed_claims,
            sealed_digest: self.sealed_digest(transaction)?,
            dropped_envelopes,
            dropped_claims,
            by_kind,
            drop_digest: self.drop_digest(transaction, true)?,
            retained_digest: self.retained_digest(transaction)?,
        };
        transaction.execute(/* sqlite-large-value: allow-for-now lv-hd46e59c170c1 owner=smallclaims::store::checkpoint_staging TODO=bound-or-reference-values-and-reclamation */ "UPDATE meta SET plan=?1 WHERE id=1", [serde_json::to_string(&plan)?])?;
        Ok(plan)
    }

    /// `sealed_digest`: the sealed envelopes' identities in key order.
    fn sealed_digest(&self, connection: &Connection) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"st3-checkpoint-sealed-v1\0");
        let mut statement = connection.prepare(
            "SELECT writer, sequence, envelope_hash FROM envelopes WHERE included=1
             ORDER BY writer, sequence, envelope_hash",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            update_identity_digest(&mut digest, &row.get::<_, String>(0)?, row.get(1)?, &row.get::<_, String>(2)?);
        }
        Ok(hex::encode(digest.finalize()))
    }

    /// `drop_digest` over every tombstone before the cut, earlier and, with `new`, this plan's,
    /// in the order a set of tombstones sorts into, without duplicates.
    fn drop_digest(&self, connection: &Connection, new: bool) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"st3-checkpoint-drop-v1\0");
        // A union with itself deduplicates the earlier rows alone as the union does.
        let envelopes = if new {
            "SELECT writer, sequence, envelope_hash, accepted_at FROM earlier_envelopes
             UNION SELECT writer, sequence, envelope_hash, accepted_at FROM dropped_envelopes"
        } else {
            "SELECT writer, sequence, envelope_hash, accepted_at FROM earlier_envelopes
             UNION SELECT writer, sequence, envelope_hash, accepted_at FROM earlier_envelopes"
        };
        let count: i64 =
            connection.query_row(/* sqlite-large-value: allow-for-now lv-h3d7356206c7d owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM ({envelopes})"), [], |row| row.get(0))?;
        digest.update((count as u64).to_be_bytes());
        let mut statement = connection.prepare(/* sqlite-large-value: allow-for-now lv-h445283bfa9a7 owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!("{envelopes} ORDER BY 1, 2, 3, 4"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            digest_field(&mut digest, Some(&row.get::<_, String>(0)?));
            digest_field(&mut digest, Some(&row.get::<_, u64>(1)?.to_string()));
            digest_field(&mut digest, Some(&row.get::<_, String>(2)?));
            digest_field(&mut digest, Some(&u128_of(row.get(3)?).to_string()));
        }
        let columns = "id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
             operation_id, request_digest, accepted_at";
        let other = if new { "dropped_claims" } else { "earlier_claims" };
        let claims = format!("SELECT {columns} FROM earlier_claims UNION SELECT {columns} FROM {other}");
        let count: i64 =
            connection.query_row(/* sqlite-large-value: allow-for-now lv-h832768d81f3e owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!("SELECT COUNT(*) FROM ({claims})"), [], |row| row.get(0))?;
        digest.update((count as u64).to_be_bytes());
        // Claim IDs are unique within each table, so the ID orders them as the tombstones' own
        // order does; the remaining columns only order one ID tombstoned twice differently.
        let mut statement =
            connection.prepare(/* sqlite-large-value: allow-for-now lv-hcb747f4debdc owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ &format!("{claims} ORDER BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let predecessors: Vec<String> =
                serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default();
            digest_field(&mut digest, Some(&row.get::<_, String>(0)?));
            digest_field(&mut digest, Some(&row.get::<_, String>(1)?));
            digest_field(&mut digest, Some(&row.get::<_, u64>(2)?.to_string()));
            digest_field(&mut digest, Some(&row.get::<_, String>(3)?));
            digest_field(&mut digest, Some(&row.get::<_, String>(4)?));
            digest_field(&mut digest, Some(&row.get::<_, String>(5)?));
            digest_field(&mut digest, row.get::<_, Option<String>>(6)?.as_deref());
            digest.update((predecessors.len() as u64).to_be_bytes());
            for predecessor in &predecessors {
                digest_field(&mut digest, Some(predecessor));
            }
            digest_field(&mut digest, row.get::<_, Option<String>>(8)?.as_deref());
            digest_field(&mut digest, row.get::<_, Option<String>>(9)?.as_deref());
            digest_field(&mut digest, Some(&u128_of(row.get(10)?).to_string()));
        }
        Ok(hex::encode(digest.finalize()))
    }

    /// `retained_digest` over the kept claims' IDs in order.
    fn retained_digest(&self, connection: &Connection) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"st3-checkpoint-retained-v1\0");
        let mut statement = connection
            .prepare("SELECT DISTINCT id FROM claims WHERE first=1 AND dropped=0 ORDER BY id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            digest_field(&mut digest, Some(&row.get::<_, String>(0)?));
        }
        Ok(hex::encode(digest.finalize()))
    }

    /// The new envelope tombstones after the `after`th, at most `limit`.
    pub fn dropped_envelopes(&self, after: usize, limit: usize) -> Result<Vec<EnvelopeTombstone>> {
        Ok(self
            .staging
            .prepare_cached(
                "SELECT writer, sequence, envelope_hash, accepted_at FROM dropped_envelopes
                 WHERE n > ?1 ORDER BY n LIMIT ?2",
            )?
            .query_map(params![i64::try_from(after)?, i64::try_from(limit)?], |row| {
                Ok(EnvelopeTombstone {
                    writer: row.get(0)?,
                    sequence: row.get(1)?,
                    envelope_hash: row.get(2)?,
                    accepted_at_unix_ms: u128_of(row.get(3)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The new claim tombstones after the `after`th, at most `limit`.
    pub fn dropped_claims(&self, after: usize, limit: usize) -> Result<Vec<ClaimTombstone>> {
        Ok(self
            .staging
            .prepare_cached(
                "SELECT id, writer, sequence, envelope_hash, subject, kind, actor, predecessors,
                        operation_id, request_digest, accepted_at
                 FROM dropped_claims WHERE n > ?1 ORDER BY n LIMIT ?2",
            )?
            .query_map(params![i64::try_from(after)?, i64::try_from(limit)?], |row| {
                Ok(ClaimTombstone {
                    id: row.get(0)?,
                    writer: row.get(1)?,
                    sequence: row.get(2)?,
                    envelope_hash: row.get(3)?,
                    subject: row.get(4)?,
                    kind: row.get(5)?,
                    actor: row.get(6)?,
                    predecessors: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
                    operation_id: row.get(8)?,
                    request_digest: row.get(9)?,
                    accepted_at_unix_ms: u128_of(row.get(10)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Whether the plan drops this claim.
    pub fn drops_claim(&self, id: &str) -> Result<bool> {
        Ok(self.staging.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE id=?1 AND first=1 AND dropped=1)",
            [id],
            |row| row.get(0),
        )?)
    }

    /// The identity a failed proof records: what `proof_input_digest` hashes, read in canonical
    /// order from the staging file.
    pub fn proof_input_digest(&self, plan: &StagedPlan, executable: &str, build: &str, protocol: u64) -> Result<String> {
        let mut digest = Sha256::new();
        digest.update(b"smallclaims-checkpoint-proof-input-v1\0");
        digest.update(protocol.to_be_bytes());
        digest.update(self.cut_unix_ms.to_be_bytes());
        let tombstones = self.drop_digest(&self.staging, false)?;
        for field in [
            plan.sealed_digest.as_str(),
            plan.drop_digest.as_str(),
            plan.retained_digest.as_str(),
            plan.rules_digest.as_str(),
            build,
            executable,
            tombstones.as_str(),
        ] {
            digest_field(&mut digest, Some(field));
        }
        let count: i64 = self.staging.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))?;
        digest.update((count as u64).to_be_bytes());
        let mut statement =
            self.staging.prepare("SELECT id, accepted_at, valid FROM claims ORDER BY pos")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            digest_field(&mut digest, Some(&row.get::<_, String>(0)?));
            digest.update(u128_of(row.get(1)?).to_be_bytes());
            digest.update([u8::from(row.get::<_, bool>(2)?)]);
        }
        Ok(hex::encode(digest.finalize()))
    }
}

/// Tombstones a proof or a trim reads from the staging file at a time.
const TOMBSTONE_PAGE: usize = 256;

/// A staged set and its plan, as a proof reads them.
pub struct StagedProof<'a> {
    pub staged: &'a StagedSet,
    pub plan: &'a StagedPlan,
}

impl ProofInput for StagedProof<'_> {
    fn cut_unix_ms(&self) -> u128 {
        self.staged.cut_unix_ms
    }

    fn seal_rowid(&self) -> i64 {
        self.staged.seal_rowid
    }

    fn sealed_digest(&self) -> &str {
        &self.plan.sealed_digest
    }

    fn rules_digest(&self) -> &str {
        &self.plan.rules_digest
    }

    fn drop_digest(&self) -> &str {
        &self.plan.drop_digest
    }

    fn attach(&self, connection: &Connection) -> Result<()> {
        connection.execute(/* sqlite-large-value: allow-for-now lv-h0f06faaf001d owner=smallclaims::store::checkpoint_staging TODO=trace-statement-and-value-representation */ "ATTACH DATABASE ?1 AS staged", [self.staged.path.to_string_lossy()])?;
        Ok(())
    }

    fn fill_sealed(&self, transaction: &Transaction<'_>) -> Result<()> {
        transaction.execute_batch(
            /* sqlite-large-value: allow-for-now lv-hb35a797d1f6e owner=smallclaims::store::checkpoint_staging TODO=chunk-or-reference-exact-wire-values */ "INSERT OR IGNORE INTO temp.sealed_claims(id) SELECT id FROM staged.claims;
             INSERT OR IGNORE INTO temp.sealed_blobs(hash) SELECT hash FROM staged.blobs;
             INSERT OR IGNORE INTO temp.sealed_envelopes(writer, sequence, envelope_hash)
                 SELECT writer, sequence, envelope_hash FROM staged.envelopes WHERE included=1;",
        )?;
        Ok(())
    }

    fn subjects(&self, after: Option<&str>, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .staged
            .staging
            .prepare_cached(
                "SELECT DISTINCT subject FROM dropped_claims WHERE ?1 IS NULL OR subject > ?1
                 ORDER BY subject LIMIT ?2",
            )?
            .query_map(params![after, i64::try_from(limit)?], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn envelope_pages(&self, visit: &mut dyn FnMut(&[EnvelopeTombstone]) -> Result<()>) -> Result<()> {
        let mut after = 0;
        loop {
            let page = self.staged.dropped_envelopes(after, TOMBSTONE_PAGE)?;
            if page.is_empty() {
                return Ok(());
            }
            visit(&page)?;
            after += page.len();
        }
    }

    fn claim_pages(&self, visit: &mut dyn FnMut(&[ClaimTombstone]) -> Result<()>) -> Result<()> {
        let mut after = 0;
        loop {
            let page = self.staged.dropped_claims(after, TOMBSTONE_PAGE)?;
            if page.is_empty() {
                return Ok(());
            }
            visit(&page)?;
            after += page.len();
        }
    }

    fn drops_claim(&self, id: &str) -> Result<bool> {
        self.staged.drops_claim(id)
    }
}

/// Remove a SQLite file and its journal files.
fn remove_file_set(path: &Path) -> Result<()> {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        match fs::remove_file(format!("{}{suffix}", path.display())) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    Ok(())
}
