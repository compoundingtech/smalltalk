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
        let result = tokio::task::spawn_blocking(move || {
            crate::profile::task("task migrate-event-payloads", || {
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

/// Source transactions do only one bounded fold. This weakly owned daemon worker
/// completes queued folds in later writer loans, including queues left by trimming.
pub async fn run_client_message_selectors(
    store: std::sync::Weak<Store>,
    notify: Arc<tokio::sync::Notify>,
    events: tokio::sync::watch::Sender<u64>,
) {
    loop {
        let Some(current) = store.upgrade() else { return; };
        let result = tokio::task::spawn_blocking(move || {
            let pending = current.client_message_selectors_pending()?;
            let more = current.maintain_client_message_selectors()?;
            let remaining = current.client_message_selectors_pending()?;
            Ok::<_, anyhow::Error>((pending, remaining, more))
        }).await;
        let more = match result {
            Ok(Ok((pending, remaining, more))) => {
                if pending && !remaining {
                    events.send_modify(|generation| *generation = generation.saturating_add(1));
                    notify.notify_one();
                }
                more
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "message selector maintenance deferred");
                false
            }
            Err(error) => {
                tracing::warn!(error = %error, "message selector maintenance task failed");
                false
            }
        };
        tokio::time::sleep(if more { Duration::from_millis(20) } else { Duration::from_secs(1) }).await;
    }
}

fn contention(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
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

    #[tokio::test]
    async fn selector_worker_drains_committed_pending_claims_notifies_and_releases_store() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("selectors.sqlite"),"node").unwrap());
        {
            let mut connection = store.connection.write();
            let transaction = connection.transaction().unwrap();
            smallclaims::store::append_claim_record_tx(
                &transaction,"node","message/maintenance","message.sent",Some("person/sender"),
                &serde_json::json!({"fields":{"from":"person/sender","to":"person/recipient","content":"queued","status":"sent","tags":[]}}),
                &[],None,
            ).unwrap();
            transaction.commit().unwrap();
        }
        assert!(store.client_message_selectors_pending().unwrap());
        let notify = Arc::new(tokio::sync::Notify::new());
        let (events,observed) = tokio::sync::watch::channel(0);
        let weak = Arc::downgrade(&store);
        let worker = tokio::spawn(run_client_message_selectors(weak.clone(),notify.clone(),events));
        tokio::time::timeout(Duration::from_secs(5),notify.notified()).await.unwrap();
        assert!(!store.client_message_selectors_pending().unwrap());
        assert_eq!(*observed.borrow(),1);
        let rows = store.read_snapshot(|through|store.client_messages_page(None,None,true,through,None,10)).unwrap();
        assert_eq!(rows.len(),1);
        assert_eq!(rows[0].0.content,"queued");
        drop(store);
        tokio::time::timeout(Duration::from_secs(3),worker).await.unwrap().unwrap();
        assert!(weak.upgrade().is_none(), "idle worker must not keep its store alive");
    }

    #[test]
    fn idle_selector_maintenance_never_borrows_or_commits_the_writer() {
        let store = Store::open_memory("node").unwrap();
        let commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = commits.clone();
        let _observer = store.connection.observe_commits(move |_| {
            count.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
        });
        assert!(!store.maintain_client_message_selectors().unwrap());
        assert!(!store.maintain_client_message_selectors().unwrap());
        assert_eq!(commits.load(std::sync::atomic::Ordering::Relaxed),0);
    }
}
