//! Controls for where a replication projection chunk's writer loan spends its hold. Each slows
//! one point inside the loan by `SLOW` and asserts that the time lands in that point's stage,
//! with every other stage and the unattributed handoff under `QUICK`. The 3x separation keeps
//! the assertions valid under CI load; none bounds wall time from above.
use super::checkpoint::{DropPlan, SealedSet};
use super::runtime::{IncrementalProjection, LegacyDigestTable, Plain};
use super::*;
use crate::error::Error;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

const SLOW: Duration = Duration::from_millis(300);
const QUICK: f64 = 100.0;

/// A plain runtime that can sleep in, divert or fail one projection step, and that records the
/// calls a chunk makes so a staged pass can be compared with an unstaged one.
#[derive(Default)]
struct StageRuntime {
    slow: Mutex<Option<&'static str>>,
    replay: AtomicBool,
    fail_incremental: AtomicBool,
    fail_replay: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl StageRuntime {
    fn pause(&self, point: &str) {
        if self.slow.lock().unwrap().is_some_and(|slow| slow == point) {
            std::thread::sleep(SLOW);
        }
    }
}

impl Runtime for StageRuntime {
    fn migrate_schema(&self, connection: &Connection) -> Result<()> {
        Plain.migrate_schema(connection)
    }

    fn create_schema(&self, connection: &Connection) -> Result<()> {
        Plain.create_schema(connection)
    }

    fn open_projections(&self, transaction: &Transaction<'_>, shared_memory: bool) -> Result<()> {
        Plain.open_projections(transaction, shared_memory)
    }

    fn schema_digest(&self) -> String {
        Plain.schema_digest()
    }

    fn classify_replicated_claim(
        &self,
        connection: &Connection,
        batch: &ReplicaBatch,
        claim: &ClaimRecord,
    ) -> Result<ReplicatedClaimAdmission, Error> {
        Plain.classify_replicated_claim(connection, batch, claim)
    }

    fn append_claim(
        &self,
        store: &Store,
        input: &ClaimInput,
    ) -> Result<(ClaimRecord, bool), Error> {
        Plain.append_claim(store, input)
    }

    fn apply_repair_tx(
        &self,
        transaction: &Transaction<'_>,
        repaired: &str,
        replacement: &str,
    ) -> Result<()> {
        Plain.apply_repair_tx(transaction, repaired, replacement)
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
        Plain.append_claim_tx(
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
        through: u64,
    ) -> Result<IncrementalProjection, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("incremental {through}"));
        self.pause("incremental");
        if self.fail_incremental.load(Ordering::Relaxed) {
            return Err(Error::new("internal", "injected incremental failure"));
        }
        if self.replay.load(Ordering::Relaxed) {
            return Ok(IncrementalProjection::Replay("test-replay"));
        }
        Ok(IncrementalProjection::Projected)
    }

    fn replay_from_nothing(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        self.calls.lock().unwrap().push("replay".into());
        self.pause("replay");
        if self.fail_replay.load(Ordering::Relaxed) {
            return Err(Error::new("internal", "injected replay failure"));
        }
        Ok(())
    }

    fn after_projection(&self, _transaction: &Transaction<'_>) -> Result<(), Error> {
        self.calls.lock().unwrap().push("after".into());
        self.pause("after-projection");
        Ok(())
    }

    fn forget_views(&self) {}

    fn digest_tables(&self) -> &'static [(&'static str, &'static [&'static str])] {
        Plain.digest_tables()
    }

    fn legacy_digest_tables(&self) -> &'static [LegacyDigestTable] {
        Plain.legacy_digest_tables()
    }

    fn checkpoint_rules_digest(&self) -> String {
        Plain.checkpoint_rules_digest()
    }

    fn plan_checkpoint_drops(&self, sealed: &SealedSet) -> DropPlan {
        Plain.plan_checkpoint_drops(sealed)
    }

    fn clear_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()> {
        Plain.clear_checkpoint_projections(transaction)
    }

    fn replay_checkpoint_projections(&self, transaction: &Transaction<'_>) -> Result<()> {
        Plain.replay_checkpoint_projections(transaction)
    }

    fn checkpoint_subject_answers(
        &self,
        connection: &Connection,
        subject: &str,
        cut: u128,
    ) -> Result<Value> {
        Plain.checkpoint_subject_answers(connection, subject, cut)
    }
}

/// A store whose graph is projected and healthy, with `pending` claims not yet projected. Holds
/// over 100 ms are kept as long holds; nextest runs each test in its own process.
fn store(runtime: &Arc<StageRuntime>, pending: usize) -> (tempfile::TempDir, Store) {
    crate::windows::set_store_target(
        crate::windows::StoreWork::WriterHold,
        Some(Duration::from_millis(100)),
    );
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("claims.sqlite3"), "node", runtime.clone()).unwrap();
    assert!(store.project_replication_backlog().unwrap());
    for index in 0..pending {
        store
            .append_claim(&ClaimInput {
                subject: format!("note/pending-{index}"),
                kind: "example.note".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    runtime.calls.lock().unwrap().clear();
    (dir, store)
}

/// Wait until the writer has recorded every hold before now: it ends a hold before it takes its
/// next job, so a write that returns follows them all.
fn settle(store: &Store) {
    store
        .connection
        .batched(|_| Ok::<_, rusqlite::Error>(()))
        .unwrap()
        .unwrap();
}

/// The staged long hold that work charged to `label` produced: the projection chunk's loan.
fn staged_row(store: &Store, label: &str) -> Value {
    settle(store);
    let report = crate::windows::holds::report();
    let rows = report["long_holds"].as_array().unwrap();
    let staged = rows
        .iter()
        .filter(|row| row["holder"] == label && row.get("stages").is_some())
        .collect::<Vec<_>>();
    assert_eq!(staged.len(), 1, "one staged loan for {label} in {report}");
    staged[0].clone()
}

fn stage(row: &Value, name: &str) -> f64 {
    row["stages"][name].as_f64().unwrap_or(0.0)
}

/// Stages plus the unattributed handoff are the whole hold, to the microsecond.
fn assert_whole(row: &Value) {
    let staged = row["stages"]
        .as_object()
        .unwrap()
        .values()
        .map(|spent| spent.as_f64().unwrap())
        .sum::<f64>();
    let whole = staged + row["unattributed_ms"].as_f64().unwrap();
    assert!(
        (whole - row["held_ms"].as_f64().unwrap()).abs() < 0.01,
        "{row}"
    );
}

/// Every stage but `slowed` stayed quick, and so did the handoff.
fn assert_only(row: &Value, slowed: &str) {
    assert!(stage(row, slowed) >= ms(SLOW), "{slowed} in {row}");
    for (name, spent) in row["stages"].as_object().unwrap() {
        if name != slowed {
            assert!(
                spent.as_f64().unwrap() < QUICK,
                "{name} beside {slowed} in {row}"
            );
        }
    }
    assert!(row["unattributed_ms"].as_f64().unwrap() < QUICK, "{row}");
    assert_whole(row);
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Run one projection pass charged to `label`, and return the staged row it left.
fn project(store: &Store, label: &str, progress: impl FnMut(ProjectionProgress)) -> Value {
    crate::performance::with_cpu(Some(label), None, || {
        store
            .project_replication_backlog_with_progress("test", progress)
            .unwrap();
    });
    staged_row(store, label)
}

#[test]
fn each_point_inside_the_loan_is_charged_to_its_own_stage() {
    for point in [
        "incremental",
        "replay",
        "after-projection",
        "health",
        "commit",
        "progress",
        "log",
        "release",
    ] {
        let runtime = Arc::new(StageRuntime::default());
        let (_dir, store) = store(&runtime, 1);
        let label = format!("test/projection-stage/{point}");
        let armed = Arc::new(AtomicBool::new(false));
        match point {
            "incremental" | "after-projection" => *runtime.slow.lock().unwrap() = Some(point),
            "replay" => {
                runtime.replay.store(true, Ordering::Relaxed);
                *runtime.slow.lock().unwrap() = Some(point);
            }
            "health" => {
                // The chunk's healthy upsert updates the existing graph row.
                let writer = store.connection.write();
                let firing = armed.clone();
                writer
                    .create_scalar_function(
                        "stage_test_pause",
                        0,
                        rusqlite::functions::FunctionFlags::SQLITE_UTF8
                            | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
                        move |_| {
                            if firing.swap(false, Ordering::AcqRel) {
                                std::thread::sleep(SLOW);
                            }
                            Ok(0)
                        },
                    )
                    .unwrap();
                writer
                    .execute_batch(
                        "CREATE TEMP TRIGGER stage_test_health BEFORE UPDATE ON projection_health
                         BEGIN SELECT stage_test_pause(); END",
                    )
                    .unwrap();
            }
            "commit" => {
                let firing = armed.clone();
                store.connection.write().commit_hook(Some(move || {
                    if firing.swap(false, Ordering::AcqRel) {
                        std::thread::sleep(SLOW);
                    }
                    false
                }));
            }
            "log" => runtime.replay.store(true, Ordering::Relaxed),
            _ => {}
        }
        let firing = armed.clone();
        let _observer = store.observe_commits(move |_| {
            if firing.swap(false, Ordering::AcqRel) {
                std::thread::sleep(SLOW);
            }
        });
        let row = if point == "log" {
            // The full-replay line goes to the caller's log sink under the writer.
            let mut slept = false;
            crate::performance::with_cpu(Some(label.as_str()), None, || {
                store
                    .project_replication_backlog_with_log("test", |_| {
                        if !std::mem::replace(&mut slept, true) {
                            std::thread::sleep(SLOW);
                        }
                    })
                    .unwrap();
            });
            staged_row(&store, &label)
        } else {
            project(&store, &label, |update| match (point, update.phase) {
                ("progress", "project-incremental") => std::thread::sleep(SLOW),
                // Armed under the writer, so the chunk's own upsert, commit or release fires it.
                ("health" | "commit", "project-incremental") => {
                    armed.store(true, Ordering::Release)
                }
                ("release", "projection-committed") => armed.store(true, Ordering::Release),
                _ => {}
            })
        };
        let expected = match point {
            "progress" | "log" => "callbacks",
            other => other,
        };
        assert_only(&row, expected);
    }
}

#[test]
fn a_chunk_that_projects_reports_every_stage_of_its_loan_and_no_other() {
    let runtime = Arc::new(StageRuntime::default());
    let (_dir, store) = store(&runtime, 1);
    *runtime.slow.lock().unwrap() = Some("after-projection");
    let row = project(&store, "test/projection-stage/whole", |_| {});
    assert_eq!(row["kind"], "lend");
    let mut stages = row["stages"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    stages.sort();
    assert_eq!(
        stages,
        [
            "after-projection",
            "begin",
            "callbacks",
            "chunk-read",
            "commit",
            "health",
            "incremental",
            "release"
        ]
    );
    assert_whole(&row);
}

#[test]
fn a_replay_is_charged_apart_from_the_progress_it_reports() {
    let runtime = Arc::new(StageRuntime::default());
    let (_dir, store) = store(&runtime, 1);
    runtime.replay.store(true, Ordering::Relaxed);
    let mut calls = 0;
    let row = project(&store, "test/projection-stage/replay-progress", |_| {
        calls += 1;
        std::thread::sleep(Duration::from_millis(100));
    });
    // project-incremental, the replay's own stage and projection-committed.
    assert_eq!(calls, 3);
    assert!(stage(&row, "callbacks") >= 300.0, "{row}");
    assert!(stage(&row, "replay") < QUICK, "{row}");
    assert_whole(&row);
}

#[test]
fn a_failed_chunk_charges_its_log_lines_rollback_and_stale_health() {
    let runtime = Arc::new(StageRuntime::default());
    let (_dir, store) = store(&runtime, 1);
    runtime.fail_incremental.store(true, Ordering::Relaxed);
    runtime.fail_replay.store(true, Ordering::Relaxed);
    let label = "test/projection-stage/failed";
    let mut lines = 0;
    crate::performance::with_cpu(Some(label), None, || {
        let projected = store
            .project_replication_backlog_with_log("test", |_| {
                lines += 1;
                std::thread::sleep(Duration::from_millis(100));
            })
            .unwrap();
        assert!(!projected);
    });
    // The incremental failure, the full-replay line and the replay failure.
    assert_eq!(lines, 3);
    let row = staged_row(&store, label);
    for name in ["incremental", "replay", "rollback", "health", "release"] {
        assert!(row["stages"].get(name).is_some(), "{name} in {row}");
    }
    assert!(stage(&row, "callbacks") >= 300.0, "{row}");
    assert!(row["unattributed_ms"].as_f64().unwrap() < QUICK, "{row}");
    assert_whole(&row);
    let status: String = store
        .readers
        .get()
        .query_row(
            "SELECT status FROM projection_health WHERE aggregate='graph'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "stale");
}

#[test]
fn a_panic_in_a_callback_still_records_the_loan_and_returns_the_writer() {
    let runtime = Arc::new(StageRuntime::default());
    let (_dir, store) = store(&runtime, 1);
    *runtime.slow.lock().unwrap() = Some("after-projection");
    let label = "test/projection-stage/panic";
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::performance::with_cpu(Some(label), None, || {
            store.project_replication_backlog_with_progress("test", |update| {
                if update.phase == "projection-committed" {
                    panic!("a progress observer failed");
                }
            })
        })
    }));
    assert!(panicked.is_err());
    let row = staged_row(&store, label);
    assert!(row["stages"].get("callbacks").is_some(), "{row}");
    assert!(row["stages"].get("release").is_none(), "{row}");
    assert_whole(&row);
    // The writer came back: a write and a later pass both run.
    store
        .append_claim(&ClaimInput {
            subject: "note/after-panic".into(),
            kind: "example.note".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(store.project_replication_backlog().unwrap());
}

#[test]
fn holds_outside_the_projection_loan_carry_no_stages() {
    let runtime = Arc::new(StageRuntime::default());
    let (_dir, store) = store(&runtime, 0);
    let label = "test/projection-stage/unstaged";
    crate::performance::with_cpu(Some(label), None, || {
        let guard = store.connection.write();
        std::thread::sleep(Duration::from_millis(150));
        drop(guard);
        // A batch inside a stage scope is not a staged lend either.
        let stages = crate::windows::holds::Stages::begin();
        stages.enter("begin");
        store
            .connection
            .batched(|_| {
                std::thread::sleep(Duration::from_millis(150));
                Ok::<_, rusqlite::Error>(())
            })
            .unwrap()
            .unwrap();
    });
    settle(&store);
    let report = crate::windows::holds::report();
    let rows = report["long_holds"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["holder"] == label)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "{report}");
    for row in rows {
        let mut keys = row.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "at",
                "client",
                "held_ms",
                "holder",
                "kind",
                "longest_write_ms",
                "started_unix_ms",
                "writes"
            ],
            "{row}"
        );
    }
}

/// What a projection pass did, for comparing a staged pass with an unstaged one.
#[derive(Debug, PartialEq)]
struct Pass {
    calls: Vec<String>,
    progress: Vec<String>,
    lines: Vec<String>,
    health: (String, Option<u64>, Option<String>),
    projected: bool,
}

fn pass(stages: bool, fail: bool) -> Pass {
    let runtime = Arc::new(StageRuntime::default());
    // More than two chunks.
    let (_dir, store) = store(&runtime, 2 * PROJECTION_CHUNK_CLAIMS + 7);
    runtime.fail_incremental.store(fail, Ordering::Relaxed);
    let mut progress = vec![];
    let mut lines = vec![];
    let mut run = || {
        store.project_replication_backlog_chunks(
            || {},
            || {},
            "test",
            |line| lines.push(line.to_owned()),
            |update| {
                progress.push(format!(
                    "{} {} {} {:?} {:?}",
                    update.phase, update.frontier, update.target, update.processed, update.total
                ))
            },
        )
    };
    let projected = if stages {
        run()
    } else {
        crate::windows::holds::unstaged(run)
    }
    .unwrap();
    let health = store
        .readers
        .get()
        .query_row(
            "SELECT status,last_good_store_index,error_code FROM projection_health WHERE aggregate='graph'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    // Log lines carry the time; compare what they say without it.
    let lines = lines
        .iter()
        .map(|line| line.split("\"at_unix_ms\"").next().unwrap().to_owned())
        .collect();
    let calls = runtime.calls.lock().unwrap().clone();
    Pass {
        calls,
        progress,
        lines,
        health,
        projected,
    }
}

#[test]
fn staging_changes_nothing_a_projection_does() {
    for fail in [false, true] {
        let staged = pass(true, fail);
        let unstaged = pass(false, fail);
        assert_eq!(staged, unstaged, "fail={fail}");
        if !fail {
            // Three chunks, each projected once, then healthy at the last index.
            assert_eq!(
                staged
                    .calls
                    .iter()
                    .filter(|call| call.starts_with("incremental"))
                    .count(),
                3
            );
            assert_eq!(staged.health.0, "healthy");
        }
    }
}
