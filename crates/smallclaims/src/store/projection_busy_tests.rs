//! Contention controls for the real projection chunk transaction and recovery path.
use super::checkpoint::{DropPlan, SealedSet};
use super::runtime::{IncrementalProjection, LegacyDigestTable, Plain};
use super::*;
use crate::error::Error;
use std::sync::atomic::AtomicI32;

struct FaultRuntime {
    stage: Mutex<Option<&'static str>>,
    fault: AtomicI32,
    replays: AtomicUsize,
}
impl FaultRuntime {
    fn new() -> Self {
        Self {
            stage: Mutex::new(None),
            fault: AtomicI32::new(0),
            replays: AtomicUsize::new(0),
        }
    }
    fn fail(&self, stage: &str) -> Result<(), Error> {
        let mut armed = self.stage.lock().unwrap();
        if armed.as_ref().is_some_and(|armed| *armed == stage) {
            *armed = None;
            return Err(internal(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(self.fault.load(Ordering::Relaxed)),
                Some("injected connection contention".into()),
            )));
        }
        Ok(())
    }
}
impl Runtime for FaultRuntime {
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
        tx: &Transaction<'_>,
        _origin: &str,
        through: u64,
    ) -> Result<IncrementalProjection, Error> {
        if *self.stage.lock().unwrap() == Some("replay") {
            return Err(Error::new(
                "internal",
                "force non-contention fallback before replay busy",
            ));
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES ('busy-test-effect',?1)",
            [through.to_string()],
        )
        .map_err(internal)?;
        self.fail("incremental")?;
        Ok(IncrementalProjection::Projected)
    }
    fn replay_from_nothing(&self, _tx: &Transaction<'_>) -> Result<(), Error> {
        self.replays.fetch_add(1, Ordering::Relaxed);
        self.fail("replay")
    }
    fn after_projection(&self, tx: &Transaction<'_>) -> Result<(), Error> {
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES ('busy-test-after','written')",
            [],
        )
        .map_err(internal)?;
        self.fail("after")
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

fn health(store: &Store) -> (String, u64) {
    store
        .readers
        .get()
        .query_row(
            "SELECT status,last_good_store_index FROM projection_health WHERE aggregate='graph'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}
fn effect(store: &Store, key: &str) -> Option<String> {
    store
        .readers
        .get()
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .unwrap()
}
fn pending_claim(store: &Store) {
    store
        .append_claim(&ClaimInput {
            subject: "note/pending".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

#[test]
fn sqlite_contention_rolls_back_without_replay_or_health_change_and_next_pass_commits() {
    for stage in ["incremental", "after"] {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
            rusqlite::ffi::SQLITE_LOCKED,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let runtime = Arc::new(FaultRuntime::new());
            let store =
                Store::open(&dir.path().join("claims.sqlite3"), "node", runtime.clone()).unwrap();
            assert!(store.project_replication_backlog().unwrap());
            let before = health(&store);
            let effect_before = effect(&store, "busy-test-effect");
            let after_before = effect(&store, "busy-test-after");
            pending_claim(&store);
            let target = store.index().unwrap();
            *runtime.stage.lock().unwrap() = Some(stage);
            runtime.fault.store(code, Ordering::Relaxed);
            let mut lines = vec![];
            assert!(
                !store
                    .project_replication_backlog_with_log("test", |line| lines
                        .push(line.to_owned()))
                    .unwrap()
            );
            assert_eq!(health(&store), before);
            assert_eq!(effect(&store, "busy-test-effect"), effect_before);
            assert_eq!(effect(&store, "busy-test-after"), after_before);
            assert!(store.replication_projection_deferred());
            assert_eq!(runtime.replays.load(Ordering::Relaxed), 0);
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with("st: projection full replay "))
            );
            assert!(store.project_replication_backlog().unwrap());
            assert_eq!(health(&store), ("healthy".into(), target));
            assert_eq!(effect(&store, "busy-test-effect"), Some(target.to_string()));
            assert!(!store.replication_projection_deferred());
            assert_eq!(runtime.replays.load(Ordering::Relaxed), 0);
        }
    }
}

#[test]
fn a_projection_chunk_owns_the_write_lock_before_observing_its_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Store::open(&path, "node", Arc::new(Plain)).unwrap();
    let competitor = Connection::open(&path).unwrap();
    competitor.busy_timeout(std::time::Duration::ZERO).unwrap();
    let mut reached = false;
    assert!(store.project_replication_backlog_with_progress("test", |progress| {
        if progress.phase == "project-incremental" {
            reached = true;
            let result = competitor.execute_batch("BEGIN IMMEDIATE");
            if result.is_ok() { competitor.execute_batch("ROLLBACK").unwrap(); }
            let error = result.expect_err("the chunk must already exclude the other SQLite writer");
            assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy));
        }
    }).unwrap());
    assert!(reached);
}

#[test]
fn a_busy_begin_preserves_health_and_deferred_work_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Store::open(&path, "node", Arc::new(Plain)).unwrap();
    assert!(store.project_replication_backlog().unwrap());
    pending_claim(&store);
    let before = health(&store);
    store
        .connection
        .write()
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let other = Connection::open(&path).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut lines = Vec::new();
    let error = store
        .project_replication_backlog_with_log("test", |line| lines.push(line.to_owned()))
        .unwrap_err();
    assert_eq!(error.downcast_ref::<Error>().unwrap().code, "database-busy");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("\"stage\":\"begin-immediate\"")
                && line.contains("\"frontier\":null"))
    );
    assert_eq!(health(&store), before);
    assert!(store.replication_projection_deferred());
    other.execute_batch("ROLLBACK").unwrap();
    assert!(store.project_replication_backlog().unwrap());
    assert_eq!(health(&store).1, store.index().unwrap());
    assert!(!store.replication_projection_deferred());
}

#[test]
fn a_non_contention_incremental_error_still_takes_the_existing_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(FaultRuntime::new());
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node", runtime.clone()).unwrap();
    assert!(store.project_replication_backlog().unwrap());
    pending_claim(&store);
    *runtime.stage.lock().unwrap() = Some("incremental");
    runtime
        .fault
        .store(rusqlite::ffi::SQLITE_ERROR, Ordering::Relaxed);
    let mut lines = vec![];
    assert!(
        store
            .project_replication_backlog_with_log("test", |line| lines.push(line.to_owned()))
            .unwrap()
    );
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 1);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("reason=incremental-error:internal"))
    );
    assert_eq!(health(&store), ("healthy".into(), store.index().unwrap()));
}

#[test]
fn serialized_checkpoint_defers_for_a_reader_and_recycles_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Store::open(&path, "node", Arc::new(Plain)).unwrap();
    pending_claim(&store);
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM claims;")
        .unwrap();
    let checkpoint = Connection::open(&path).unwrap();
    assert!(!store.checkpoint_idle_wal(&checkpoint).unwrap());
    // The failed recycle does not retain the queue loan or discard a normal write.
    pending_claim(&store);
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(store.checkpoint_idle_wal(&checkpoint).unwrap());
    assert_eq!(
        std::fs::metadata(path.with_extension("sqlite3-wal"))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        store
            .readers
            .get()
            .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn truncate_requests_the_fifo_writer_before_changing_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Arc::new(Store::open(&path, "node", Arc::new(Plain)).unwrap());
    pending_claim(&store);
    // Borrowing the writer without beginning a transaction excludes queued callers but
    // does not hold a SQLite write lock. An off-queue TRUNCATE would therefore succeed.
    let mut held = store.connection.write();
    let (observe, requests) = std::sync::mpsc::channel();
    let original = store
        .connection
        .jobs
        .lock()
        .unwrap()
        .replace(observe)
        .unwrap();
    let worker_store = store.clone();
    let worker_path = path.clone();
    let worker = std::thread::spawn(move || {
        let checkpoint = Connection::open(worker_path).unwrap();
        worker_store
            .checkpoint_idle_wal_report(&checkpoint)
            .unwrap()
    });
    let job = requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("TRUNCATE must ask the FIFO writer");
    let crate::sqlite::WriterJob::Lend { lent, returned } = job else {
        panic!("expected a writer loan");
    };
    assert!(
        std::fs::metadata(path.with_extension("sqlite3-wal"))
            .unwrap()
            .len()
            > 0
    );
    lent.send(held.connection.take().unwrap()).unwrap();
    held.connection = Some(
        returned
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
    );
    *store.connection.jobs.lock().unwrap() = Some(original);
    drop(held);
    let report = worker.join().unwrap();
    assert!(report.recycled);
    assert!(report.truncate_ms.is_some());
    assert_eq!(
        std::fs::metadata(path.with_extension("sqlite3-wal"))
            .unwrap()
            .len(),
        0
    );
    // The connection was returned to the real queue, which still commits ordinary writes.
    pending_claim(&store);
}

#[test]
fn replay_contention_logs_its_boundary_and_rolls_back_without_staling_health() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(FaultRuntime::new());
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node", runtime.clone()).unwrap();
    store.project_replication_backlog().unwrap();
    let before = health(&store);
    pending_claim(&store);
    *runtime.stage.lock().unwrap() = Some("replay");
    runtime
        .fault
        .store(rusqlite::ffi::SQLITE_BUSY, Ordering::Relaxed);
    let mut lines = Vec::new();
    assert!(
        !store
            .project_replication_backlog_with_log("test", |line| lines.push(line.to_owned()))
            .unwrap()
    );
    assert!(
        lines.iter().any(
            |line| line.contains("\"stage\":\"full-replay\"") && line.contains("database-busy")
        )
    );
    assert_eq!(health(&store), before);
    assert!(store.replication_projection_deferred());
    assert_eq!(store.projection_contention_generation(), 1);
    assert!(store.project_replication_backlog().unwrap());
    assert_eq!(runtime.replays.load(Ordering::Relaxed), 1);
}

#[test]
fn empty_wal_checkpoint_needs_no_writer_loan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claims.sqlite3");
    let store = Arc::new(Store::open(&path, "node", Arc::new(Plain)).unwrap());
    pending_claim(&store);
    let checkpoint = Connection::open(&path).unwrap();
    assert!(store.checkpoint_idle_wal(&checkpoint).unwrap());
    let held = store.connection.write();
    let target = store.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let connection = Connection::open(path).unwrap();
        let report = target.checkpoint_idle_wal_report(&connection).unwrap();
        sent.send(report).unwrap();
    });
    let report = received.recv_timeout(std::time::Duration::from_secs(2));
    drop(held); // Cleanup also releases an old implementation that tried to borrow the writer.
    worker.join().unwrap();
    let report = report.expect("an empty WAL must not wait behind a writer loan");
    assert_eq!(report.frames, 0);
    assert_eq!(report.writer_wait_ms, 0);
    assert_eq!(report.truncate_ms, None);
}
