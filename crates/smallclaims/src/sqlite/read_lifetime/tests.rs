use super::super::*;

#[test]
fn returning_a_manual_read_transaction_unpins_wal_and_reuses_a_clean_reader() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lifetime.sqlite3");
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES (zeroblob(4096));",
        )
        .unwrap();
    let pool = ReadPool::new(&path, false).unwrap();
    let reader = pool.get();
    reader.execute_batch("BEGIN").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT length(value) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4096
    );
    writer
        .execute_batch("UPDATE payload SET value=zeroblob(8192)")
        .unwrap();
    let blocked = checkpoint_wal_report(&writer).unwrap();
    assert!(
        blocked.frames > blocked.backfilled,
        "the live reader must really pin WAL"
    );
    drop(reader);
    let released = checkpoint_wal_report(&writer).unwrap();
    eprintln!(
        "reader-return fixture: before frames={} backfilled={}; after frames={} backfilled={}",
        blocked.frames, blocked.backfilled, released.frames, released.backfilled
    );
    assert_eq!(
        released.frames, released.backfilled,
        "a returned read guard must not leave an idle WAL pin"
    );
    let reused = pool.get();
    assert!(reused.is_autocommit());
    assert_eq!(
        reused
            .query_row("SELECT length(value) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        8192
    );
    assert_eq!(
        pool.usage().opened,
        1,
        "a successfully rolled-back reader is reusable"
    );
}

fn file_pool() -> (tempfile::TempDir, Connection, ReadPool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("reader.sqlite3");
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
        CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES(zeroblob(4096));",
        )
        .unwrap();
    let pool = ReadPool::new(&path, false).unwrap();
    (directory, writer, pool)
}

#[test]
fn failed_rollback_discards_the_physical_reader() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let (_directory, writer, pool) = file_pool();
    let reader = pool.get();
    reader
        .execute_batch("BEGIN; SELECT * FROM payload")
        .unwrap();
    writer
        .execute_batch("UPDATE payload SET value=zeroblob(8192)")
        .unwrap();
    reader.authorizer(Some(|context: AuthContext<'_>| {
        if matches!(context.action, AuthAction::Transaction { .. }) {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }));
    drop(reader);
    assert_eq!((pool.usage().open, pool.usage().idle), (0, 0));
    let report = checkpoint_wal_report(&writer).unwrap();
    assert_eq!(report.frames, report.backfilled);
    let reader = pool.get();
    assert!(reader.is_autocommit());
    assert_eq!(pool.usage().opened, 2);
}

#[test]
fn error_and_panic_return_only_clean_ordinary_readers() {
    let (_directory, writer, pool) = file_pool();
    let budget =
        crate::read_budget::ReadBudget::new("/test-reader", std::time::Duration::from_secs(30));
    let failure: Result<()> = crate::read_budget::with(Some(budget.clone()), || {
        let reader = pool.get();
        reader
            .execute_batch("BEGIN; SELECT * FROM payload")
            .unwrap();
        writer
            .execute_batch("UPDATE payload SET value=zeroblob(8192)")
            .unwrap();
        budget.cancel();
        crate::read_budget::check()?;
        Ok(())
    });
    assert!(failure.is_err());
    assert!(pool.get().is_autocommit());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let reader = pool.get();
        reader
            .execute_batch("BEGIN; SELECT * FROM payload")
            .unwrap();
        writer
            .execute_batch("UPDATE payload SET value=zeroblob(16384)")
            .unwrap();
        panic!("worker unwind");
    }));
    assert!(panic.is_err());
    let report = checkpoint_wal_report(&writer).unwrap();
    assert_eq!(report.frames, report.backfilled);
    let reader = pool.get();
    assert!(reader.is_autocommit());
    assert_eq!(
        reader
            .query_row("SELECT length(value) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        16384
    );
    assert_eq!(pool.usage().opened, 1);
    assert_eq!(pool.live_read_report().snapshot_checkouts, 0);
}

#[test]
fn nested_snapshot_drop_preserves_outer_cut_and_error_panic_cleanup() {
    use crate::store::{Store, runtime::Plain};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("snapshot.sqlite3");
    let store = Store::open(&path, "reader", Arc::new(Plain)).unwrap();
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "PRAGMA wal_autocheckpoint=0; CREATE TABLE payload(value INTEGER);
        INSERT INTO payload VALUES(1)",
        )
        .unwrap();
    let value = || {
        store
            .readers
            .get()
            .query_row("SELECT value FROM payload", [], |row| row.get::<_, i64>(0))
            .unwrap()
    };
    store
        .read_snapshot(|index| {
            assert_eq!(value(), 1);
            let first = store.readers.live_read_report().oldest_snapshot.unwrap();
            assert!(first.snapshot_confirmed_age_ms.is_some());
            assert!(first.connection_id.is_some());
            writer.execute_batch("UPDATE payload SET value=2").unwrap();
            store.read_snapshot(|nested| {
                assert_eq!(nested, index);
                assert_eq!(value(), 1);
                Ok(())
            })?;
            assert_eq!(
                value(),
                1,
                "nested and ordinary guard drops must retain the outer cut"
            );
            let report = checkpoint_wal_report(&writer).unwrap();
            assert!(report.frames > report.backfilled);
            Ok(())
        })
        .unwrap();
    assert_eq!(value(), 2);
    let failed: Result<()> = store.read_snapshot(|_| {
        writer.execute_batch("UPDATE payload SET value=3").unwrap();
        anyhow::bail!("snapshot error")
    });
    assert!(failed.is_err());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<()> = store.read_snapshot(|_| {
            writer.execute_batch("UPDATE payload SET value=4").unwrap();
            panic!("snapshot unwind");
        });
    }));
    assert!(panic.is_err());
    assert_eq!(value(), 4);
    assert!(store.readers.get().is_autocommit());
    assert_eq!(store.readers.live_read_report().live_checkouts, 0);
    let report = checkpoint_wal_report(&writer).unwrap();
    assert_eq!(report.frames, report.backfilled);
}

#[test]
fn canceled_worker_is_reported_until_legitimate_return_then_unpins() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (_directory, writer, pool) = file_pool();
    let pool = Arc::new(pool);
    let budget =
        crate::read_budget::ReadBudget::new("/test-canceled-worker", Duration::from_secs(30));
    let (ready, started) = mpsc::channel();
    let (finish, finishing) = mpsc::channel();
    let worker_pool = pool.clone();
    let worker_budget = budget.clone();
    let worker = std::thread::spawn(move || {
        crate::read_budget::with(Some(worker_budget), || {
            worker_pool
                .request_read(|| {
                    worker_pool
                        .get()
                        .execute_batch("BEGIN; SELECT * FROM payload")
                        .unwrap();
                    ready.send(()).unwrap();
                    finishing.recv_timeout(Duration::from_secs(30)).unwrap();
                    assert!(crate::read_budget::check().is_err());
                })
                .unwrap();
        })
    });
    started.recv_timeout(Duration::from_secs(30)).unwrap();
    budget.cancel();
    writer
        .execute_batch("UPDATE payload SET value=zeroblob(8192)")
        .unwrap();
    let candidate = pool.live_read_report();
    assert_eq!(
        (candidate.live_checkouts, candidate.snapshot_checkouts),
        (1, 0)
    );
    let loan = candidate.oldest_loan.unwrap();
    assert_eq!(loan.route.as_deref(), Some("/test-canceled-worker"));
    assert_eq!(loan.budget_expired, Some(true));
    assert!(loan.connection_id.is_some());
    assert_eq!(
        loan.snapshot_confirmed_age_ms, None,
        "raw transaction age is unknown"
    );
    let report = checkpoint_wal_report(&writer).unwrap();
    assert!(
        report.frames > report.backfilled,
        "request cancellation alone does not end worker ownership"
    );
    finish.send(()).unwrap();
    worker.join().unwrap();
    assert_eq!(pool.live_read_report().live_checkouts, 0);
    let report = checkpoint_wal_report(&writer).unwrap();
    assert_eq!(report.frames, report.backfilled);
    assert!(pool.get().is_autocommit());
    assert_eq!(pool.usage().opened, 1);
}

#[test]
fn diagnostics_scope_pool_and_distinguish_checkout_from_known_snapshot() {
    let (_a, _aw, pool) = file_pool();
    let (_b, _bw, other) = file_pool();
    let unrelated = other.get();
    let budget =
        crate::read_budget::ReadBudget::new("x".repeat(400), std::time::Duration::from_secs(30));
    crate::read_budget::with(Some(budget), || {
        let reader = pool.get();
        let pending = pool.register_snapshot();
        let report = pool.live_read_report();
        assert_eq!((report.live_checkouts, report.snapshot_checkouts), (2, 1));
        assert_eq!(
            report.oldest_snapshot.unwrap().snapshot_confirmed_age_ms,
            None
        );
        let first = report.oldest_loan.unwrap();
        assert_eq!(first.route.unwrap().len(), 256);
        assert_eq!(first.budget_expired, Some(false));
        reader
            .execute_batch("BEGIN; SELECT * FROM payload")
            .unwrap();
        pending.snapshot_started(reader.connection.as_ref().unwrap());
        let snapshot = pool.live_read_report().oldest_snapshot.unwrap();
        assert_eq!(snapshot.connection_id, first.connection_id);
        assert!(snapshot.snapshot_confirmed_age_ms.is_some());
        drop(reader);
        drop(pending);
        let reused = pool.get();
        let second = pool.live_read_report().oldest_loan.unwrap();
        assert_eq!(second.connection_id, first.connection_id);
        assert_ne!(second.read_id, first.read_id);
        drop(reused);
    });
    assert_eq!(pool.live_read_report().live_checkouts, 0);
    assert_eq!(other.live_read_report().live_checkouts, 1);
    drop(unrelated);
}

#[test]
fn ordinary_reuse_and_reporting_add_no_sql_per_checkout() {
    let (_directory, _writer, pool) = file_pool();
    let before_query = STATEMENTS_RUN.with(|count| count.get());
    assert_eq!(
        pool.get()
            .query_row("SELECT length(value) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4096
    );
    let before = STATEMENTS_RUN.with(|count| count.get());
    assert!(
        before > before_query,
        "the statement counter must actually observe SQL"
    );
    let mut physical = None;
    for _ in 0..100 {
        let reader = pool.get();
        assert!(reader.is_autocommit());
        let report = pool.live_read_report();
        assert_eq!(report.live_checkouts, 1);
        assert!(report.oldest_snapshot.is_none());
        let current = report.oldest_loan.unwrap().connection_id;
        if let Some(previous) = physical {
            assert_eq!(current, previous);
        }
        physical = Some(current);
        drop(reader);
    }
    assert_eq!(STATEMENTS_RUN.with(|count| count.get()), before);
    assert_eq!((pool.usage().open, pool.usage().opened), (1, 1));
    assert_eq!(pool.live_read_report().live_checkouts, 0);
}
