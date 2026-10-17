//! Bounded maintenance uses the same single-writer transactions as normal requests.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::store::Store;

/// Wall duration includes pauses, retries and scheduling. Chunk calls include writer waiting
/// and the transaction's commit; they are not measurements of exclusive writer hold or CPU.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EventMigrationReport {
    pub pending_at_start: bool,
    pub completed: bool,
    pub moved_rows: u64,
    pub chunks: u64,
    pub elapsed_ms: f64,
    pub chunk_call_ms: f64,
    pub max_chunk_call_ms: f64,
    pub retries: u64,
}

/// Drain admitted legacy membership, at most 64 rows per commit and with a 20 ms pause.
/// Daemon startup and the load fixture call this worker, rather than separate drain loops.
pub async fn migrate_event_payloads(store: Arc<Store>) -> Result<EventMigrationReport> {
    let started = Instant::now();
    let mut report = EventMigrationReport::default();
    let mut retry = Duration::from_millis(200);
    let mut first = true;
    loop {
        let store = store.clone();
        let result = store.connection.write_workers().spawn_blocking(move || {
            crate::task_cpu::profiled_blocking("maintenance.event_payloads", || {
                let pending = store.event_payload_migration_pending()?;
                let chunk_started = Instant::now();
                let result = store.migrate_event_payloads();
                Ok::<_, anyhow::Error>((pending, result, chunk_started.elapsed()))
            })
        })
        .await?;
        let (pending, result, elapsed) = match result {
            Ok(call) => call,
            Err(error) if contention(&error) => {
                report.retries += 1;
                eprintln!("st3: event migration read contention; retrying in {retry:?}: {error:#}");
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(5));
                continue;
            }
            Err(error) => return Err(error),
        };
        if first {
            report.pending_at_start = pending;
            first = false;
        }
        let ms = elapsed.as_secs_f64() * 1_000.0;
        report.chunk_call_ms += ms;
        report.max_chunk_call_ms = report.max_chunk_call_ms.max(ms);
        match result {
            Ok(moved) => {
                if moved == 0 {
                    report.completed = true;
                    report.elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
                    return Ok(report);
                }
                report.moved_rows += moved as u64;
                report.chunks += 1;
                retry = Duration::from_millis(200);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) if contention(&error) => {
                report.retries += 1;
                eprintln!("st3: event migration contention; retrying in {retry:?}: {error:#}");
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn contention(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
}

/// Turns on the projection divergence check: how often, in milliseconds, it looks. Unset or zero,
/// it never looks; anything under an hour looks hourly.
pub const DIVERGENCE_CHECK_ENV: &str = "ST3_DIVERGENCE_CHECK_INTERVAL_MS";
const DIVERGENCE_CHECK_MIN: Duration = Duration::from_secs(60 * 60);
/// The first wait while the daemon is busy; it doubles until the next check is due.
const DIVERGENCE_BUSY_BACKOFF: Duration = Duration::from_secs(60);

/// How often the divergence check runs on this daemon, or `None` when it is off.
pub fn divergence_check_interval(value: Option<&str>) -> Option<Duration> {
    let ms = value?.trim().parse::<u64>().ok().filter(|ms| *ms > 0)?;
    Some(Duration::from_millis(ms).max(DIVERGENCE_CHECK_MIN))
}

/// Whether the daemon is too busy for background checks: the store's writer or a write
/// transaction ran over its target in the last minute, or replication is catching up or has
/// put projection off.
pub fn daemon_busy(store: &Store) -> bool {
    use smallclaims::windows::{StoreWork, store_series};
    let now = Instant::now();
    let max_ms = crate::slo::targets().transaction.max_ms as f64;
    let over = |kind| {
        let minute = &store_series(kind).snapshot(now)["1m"];
        minute["over_target"].as_u64().unwrap_or(0) > 0
            || minute["max_ms"].as_f64().unwrap_or(0.0) > max_ms
    };
    over(StoreWork::WriterHold)
        || over(StoreWork::WriteTransaction)
        || store.replication_catching_up()
        || store.replication_projection_deferred()
}

/// Look for projection divergences every `interval`, skipping a round the daemon stays busy
/// through. Each check reads comparisons exchanges already made and the open faults; it records a
/// fault for each new divergence and ends each fault whose members agree again.
pub async fn check_projection_divergences(store: Arc<Store>, interval: Duration) {
    store.capture_divergences(true);
    loop {
        tokio::time::sleep(interval).await;
        let mut backoff = DIVERGENCE_BUSY_BACKOFF;
        while daemon_busy(&store) && backoff < interval {
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
        if daemon_busy(&store) {
            continue;
        }
        let checked = store.clone();
        let started = Instant::now();
        let result =
            tokio::task::spawn_blocking(move || checked.check_projection_divergences()).await;
        let elapsed = started.elapsed();
        match result {
            Ok(Ok(check)) if check.opened + check.recovered > 0 => eprintln!(
                "st3: projection divergence check in {elapsed:?}: {} divergent, {} opened, {} ended",
                check.divergences, check.opened, check.recovered
            ),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("st3: projection divergence check failed: {error}"),
            Err(error) => eprintln!("st3: projection divergence check stopped: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClaimInput;

    fn legacy() -> (tempfile::TempDir, Arc<Store>) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("claims.sqlite3");
        let store = Store::open(&path, "node").unwrap();
        for number in 0..65 {
            store
                .append_claim(&ClaimInput {
                    subject: format!("custom/test/migration-{number}"),
                    kind: "custom.test.recorded".into(),
                    actor: None,
                    fields: Default::default(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        store
            .connection
            .write()
            .execute_batch(
                "BEGIN;
             CREATE TABLE legacy_fixture_events AS SELECT * FROM events;
             DROP VIEW events;
             ALTER TABLE legacy_fixture_events RENAME TO events;
             DROP TABLE event_positions;
             PRAGMA user_version=16;
             COMMIT;",
            )
            .unwrap();
        drop(store);
        (root, Arc::new(Store::open(&path, "node").unwrap()))
    }

    #[tokio::test]
    async fn migration_worker_starts_drains_and_reports_completion_and_cost() {
        let (_root, store) = legacy();
        assert!(store.event_payload_migration_pending().unwrap());
        let before = store.events_after(0, None).unwrap();
        let report = migrate_event_payloads(store.clone()).await.unwrap();
        assert!(report.pending_at_start && report.completed);
        assert_eq!((report.moved_rows, report.chunks), (65, 2));
        assert!(
            report.elapsed_ms >= 40.0,
            "two 20ms inter-chunk pauses are included"
        );
        assert!(report.chunk_call_ms >= report.max_chunk_call_ms);
        assert!(!store.event_payload_migration_pending().unwrap());
        let after = store.events_after(0, None).unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
        let again = migrate_event_payloads(store).await.unwrap();
        assert!(again.completed && !again.pending_at_start);
        assert_eq!((again.moved_rows, again.chunks), (0, 0));
    }

    #[tokio::test]
    async fn migration_worker_retries_busy_and_finishes_after_writer_release() {
        let (root, store) = legacy();
        store
            .connection
            .write()
            .busy_timeout(Duration::ZERO)
            .unwrap();
        let blocker = rusqlite::Connection::open(root.path().join("claims.sqlite3")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (notice, mut changed) = tokio::sync::watch::channel(());
        let _observer = store.observe_commits(move |_| {
            notice.send_replace(());
        });
        let task = tokio::spawn(migrate_event_payloads(store.clone()));
        tokio::time::timeout(Duration::from_secs(5), changed.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(!task.is_finished());
        assert!(store.event_payload_migration_pending().unwrap());
        blocker.execute_batch("ROLLBACK").unwrap();
        let report = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(report.completed && report.retries > 0);
        assert_eq!(report.moved_rows, 65);
    }

    #[tokio::test]
    async fn migration_worker_reports_permanent_failure_without_claiming_completion() {
        let (_root, store) = legacy();
        store
            .connection
            .write()
            .execute_batch("DROP TABLE event_positions")
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            migrate_event_payloads(store.clone()),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(store.event_payload_migration_pending().unwrap());
    }
}
