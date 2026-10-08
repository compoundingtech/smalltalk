use super::*;
use crate::read_budget::{self, ReadBudget};
use crate::store::runtime::Plain;
use crate::store::Store;
use futures_util::poll;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Barrier;
use std::task::Poll;
use std::time::Duration;

fn pool(limit: usize) -> (tempfile::TempDir, Arc<ReadPool>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read.sqlite");
    Connection::open(&path).unwrap();
    let pool = with_read_limit_for_test(limit, || ReadPool::new(&path, false)).unwrap();
    (directory, Arc::new(pool))
}

#[test]
fn admitted_workers_cannot_exceed_the_configured_bound() {
    for limit in [1, 32] {
        let (_directory, pool) = pool(limit);
        let permits: Vec<_> = (0..limit).map(|_| pool.try_admit_read().unwrap()).collect();
        assert!(pool.try_admit_read().is_none());
        let release = Barrier::new(limit + 1);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            for permit in permits {
                let pool = &pool;
                let release = &release;
                let entered_tx = entered_tx.clone();
                scope.spawn(move || {
                    pool.request_read_with_permit(permit, || {
                        assert_eq!(pool.get().query_row("SELECT 1", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
                        entered_tx.send(()).unwrap();
                        release.wait();
                    }).unwrap();
                });
            }
            for _ in 0..limit {
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            assert_eq!(pool.usage().open, limit);
            assert_eq!(pool.usage().peak, limit);
            assert!(pool.try_admit_read().is_none());
            release.wait();
        });
        let permits: Vec<_> = (0..limit).map(|_| pool.try_admit_read().unwrap()).collect();
        assert!(pool.try_admit_read().is_none());
        drop(permits);
        assert_eq!(pool.usage().idle, limit.min(max_idle_read_connections()));
    }
}

#[test]
fn real_memory_store_reuses_nested_request_and_pinned_readers_at_one() {
    let store = with_read_limit_for_test(1, || Store::open_memory("reader", Arc::new(Plain))).unwrap();
    assert!(!store.readers.has_request_reader());
    store.readers.request_read(|| {
        assert!(store.readers.has_request_reader());
        assert!(store.readers.try_admit_read().is_none());
        store.readers.request_read(|| {
            store.read_snapshot(|_| {
                store.readers.request_read(|| {
                    assert!(store.readers.has_request_reader());
                    assert!(!store.readers.get().is_autocommit());
                })?;
                store.read_snapshot(|_| Ok(()))
            }).unwrap();
        }).unwrap();
        assert!(store.readers.get().is_autocommit());
        assert!(store.readers.try_admit_read().is_none());
    }).unwrap();
    assert!(!store.readers.has_request_reader());
    assert_eq!(store.readers.usage().opened, 1);
    assert!(store.readers.try_admit_read().is_some());

    // A legacy snapshot already owns its connection. It must not wait for capacity
    // held elsewhere just to enter a reentrant request scope on this thread.
    let held = store.readers.try_admit_read().unwrap();
    store.read_snapshot(|_| {
        store.readers.request_read(|| assert!(!store.readers.get().is_autocommit()))?;
        Ok(())
    }).unwrap();
    drop(held);
    assert_eq!(store.readers.usage().opened, 1);
}

#[tokio::test]
async fn a_returned_connection_is_reusable_before_the_next_admission_wakes() {
    let (_directory, pool) = pool(1);
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker_pool = pool.clone();
    let worker_entered = entered.clone();
    let worker_release = release.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let permit = worker_pool.admit_read_sync().unwrap();
        worker_pool.request_read_with_permit(permit, || {
            assert!(worker_pool.get().query_row("SELECT 1", [], |row| row.get::<_, i64>(0)).is_ok());
            worker_entered.wait();
            worker_release.wait();
        }).unwrap();
    });
    entered.wait();
    let mut queued = Box::pin(pool.admit_read(None));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    release.wait();
    worker.await.unwrap();
    let permit = queued.await.unwrap();
    // The waiter must reuse the returned idle connection, not open a transient bound+1 one.
    pool.request_read_with_permit(permit, || {
        assert_eq!((pool.usage().open, pool.usage().idle, pool.usage().opened), (1, 0, 1));
    }).unwrap();
    let permit = pool.try_admit_read().expect("the returned permit freed capacity");
    pool.request_read_with_permit(permit, || {
        assert!(pool.get().query_row("SELECT 1", [], |row| row.get::<_, i64>(0)).is_ok());
    }).unwrap();
    // Successive request scopes retain the returned connection idle, not closed.
    assert_eq!((pool.usage().open, pool.usage().idle, pool.usage().opened), (1, 1, 1));
}

#[test]
fn cross_pool_nesting_reuses_outer_readers_at_bound_one() {
    let first = with_read_limit_for_test(1, || Store::open_memory("first", Arc::new(Plain))).unwrap();
    let second = with_read_limit_for_test(1, || Store::open_memory("second", Arc::new(Plain))).unwrap();
    first.readers.request_read(|| {
        assert!(first.readers.has_request_reader());
        second.readers.request_read(|| {
            assert!(second.readers.has_request_reader());
            assert!(!first.readers.has_request_reader(), "the outer loan is not the innermost one");
            // Reentering the outer pool must neither open a second connection nor wait on
            // the capacity that pool's own outer loan already holds.
            first.readers.request_read(|| {
                assert!(first.readers.has_request_reader());
                assert!(first.readers.try_admit_read().is_none());
                assert_eq!(first.readers.usage().open, 1);
            }).unwrap();
            assert!(!first.readers.has_request_reader());
            assert!(second.readers.has_request_reader());
        }).unwrap();
        // The inner loan's exit restored this outer loan, not an empty slot.
        assert!(first.readers.has_request_reader());
        assert_eq!(first.readers.usage().open, 1);
    }).unwrap();
    assert!(!first.readers.has_request_reader());
    assert!(!second.readers.has_request_reader());
    assert_eq!(first.readers.usage().opened, 1);
    assert_eq!(second.readers.usage().opened, 1);
}

#[test]
fn outer_snapshots_are_reused_when_reentered_below_another_store_at_bound_one() {
    let first = with_read_limit_for_test(1, || Store::open_memory("first", Arc::new(Plain))).unwrap();
    let second = with_read_limit_for_test(1, || Store::open_memory("second", Arc::new(Plain))).unwrap();
    first.readers.request_read(|| {
        first.read_snapshot(|first_outer| {
            second.readers.request_read(|| {
                second.read_snapshot(|_| {
                    // The second store's pin is innermost. Reentering the first store must
                    // reuse its outer snapshot transaction, not BEGIN its loan again.
                    first.read_snapshot(|first_inner| {
                        assert_eq!(first_inner, first_outer);
                        assert!(!first.readers.get().is_autocommit());
                        Ok(())
                    })?;
                    Ok(())
                }).unwrap();
                assert!(second.readers.get().is_autocommit());
                Ok(())
            }).unwrap();
            Ok(())
        }).unwrap();
    }).unwrap();
    assert_eq!(first.readers.usage().opened, 1);
    assert_eq!(second.readers.usage().opened, 1);
    assert_eq!((first.readers.usage().open, first.readers.usage().idle), (1, 1));
    assert_eq!((second.readers.usage().open, second.readers.usage().idle), (1, 1));
    first.readers.request_read(|| assert!(first.readers.get().is_autocommit())).unwrap();
    second.readers.request_read(|| assert!(second.readers.get().is_autocommit())).unwrap();
}

#[test]
fn bare_snapshots_reenter_the_outer_store_transaction_below_another_store() {
    let first = with_read_limit_for_test(1, || Store::open_memory("first", Arc::new(Plain))).unwrap();
    let second = with_read_limit_for_test(1, || Store::open_memory("second", Arc::new(Plain))).unwrap();
    assert!(!first.readers.has_pinned_reader());
    first.read_snapshot(|first_outer| {
        assert!(first.readers.has_pinned_reader());
        second.read_snapshot(|_| {
            // The outer store's pin is hidden below the second store's, yet membership and
            // get() must both still see it; the mirror alone would not.
            assert!(first.readers.has_pinned_reader());
            assert!(second.readers.has_pinned_reader());
            first.read_snapshot(|first_inner| {
                assert_eq!(first_inner, first_outer);
                assert!(!first.readers.get().is_autocommit());
                Ok(())
            })?;
            Ok(())
        }).unwrap();
        assert!(!second.readers.has_pinned_reader());
        assert!(first.readers.has_pinned_reader());
        Ok(())
    }).unwrap();
    assert!(!first.readers.has_pinned_reader());
    assert!(!second.readers.has_pinned_reader());
    assert_eq!(first.readers.usage().opened, 1);
    assert_eq!(second.readers.usage().opened, 1);
    assert_eq!((first.readers.usage().open, first.readers.usage().idle), (1, 1));
    assert_eq!((second.readers.usage().open, second.readers.usage().idle), (1, 1));
    assert!(first.readers.get().is_autocommit());
    assert!(second.readers.get().is_autocommit());
}

#[tokio::test]
async fn queued_cancellation_and_parent_cancellation_never_obtain_a_connection() {
    for cancel_parent in [false, true] {
        let (_directory, pool) = pool(1);
        let held = pool.try_admit_read().unwrap();
        let parent = ReadBudget::new("/queued", Duration::from_secs(30));
        let budget = if cancel_parent { parent.child(Duration::from_secs(15)) } else { parent.clone() };
        let mut queued = Box::pin(pool.admit_read(Some(budget)));
        assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
        assert_eq!(pool.usage().idle, 1);
        assert_eq!(pool.usage().opened, 1);
        parent.cancel();
        let error = queued.await.err().expect("queued admission must be cancelled");
        assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
        assert_eq!(pool.usage().idle, 1);
        assert_eq!(pool.usage().opened, 1);
        assert!(pool.try_admit_read().is_none());
        drop(held);
        assert!(pool.try_admit_read().is_some());
    }
}

#[tokio::test]
async fn dropping_a_queued_future_removes_its_capacity_claim() {
    let (_directory, pool) = pool(1);
    let held = pool.try_admit_read().unwrap();
    let mut queued = Box::pin(pool.admit_read(None));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    drop(queued);
    drop(held);
    assert!(pool.try_admit_read().is_some());
    assert_eq!(pool.usage().opened, 1);
}

#[tokio::test]
async fn queued_worker_receives_capacity_only_after_the_previous_permit_drops() {
    let (_directory, pool) = pool(1);
    let held = pool.try_admit_read().unwrap();
    let mut queued = Box::pin(pool.admit_read(None));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    drop(held);
    let permit = queued.await.unwrap();
    assert!(pool.try_admit_read().is_none());
    pool.request_read_with_permit(permit, || assert!(pool.has_request_reader())).unwrap();
    assert!(pool.try_admit_read().is_some());
}

#[tokio::test]
async fn expired_admission_and_cancelled_permit_return_capacity_without_lending() {
    let (_directory, pool) = pool(1);
    let error = pool.admit_read(Some(ReadBudget::new("/expired", Duration::ZERO)))
        .await.err().expect("expired admission must fail");
    assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
    let permit = pool.admit_read(None).await.unwrap();
    let budget = ReadBudget::new("/cancelled", Duration::from_secs(30));
    budget.cancel();
    let error = read_budget::with(Some(budget), || {
        pool.request_read_with_permit(permit, || panic!("cancelled work must not start"))
    }).unwrap_err();
    assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
    assert_eq!(pool.usage().idle, 1);
    assert_eq!(pool.usage().opened, 1);
    assert!(pool.try_admit_read().is_some());
}

#[tokio::test(start_paused = true)]
async fn queued_deadline_returns_capacity_without_obtaining_a_connection() {
    let (_directory, pool) = pool(1);
    let held = pool.try_admit_read().unwrap();
    let mut queued = Box::pin(pool.admit_read(Some(ReadBudget::new("/deadline", Duration::from_millis(50)))));
    assert!(matches!(poll!(queued.as_mut()), Poll::Pending));
    let error = queued.await.err().expect("queued deadline must fail");
    assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
    assert_eq!(pool.usage().idle, 1);
    assert_eq!(pool.usage().opened, 1);
    drop(held);
    assert!(pool.try_admit_read().is_some());
}

#[test]
fn synchronous_admission_is_cancellable_without_a_tokio_runtime() {
    let (_directory, pool) = pool(1);
    let held = pool.try_admit_read().unwrap();
    let parent = ReadBudget::new("/sync", Duration::from_secs(30));
    let child = parent.child(Duration::from_secs(15));
    let worker_pool = pool.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = read_budget::with(Some(child), || {
            worker_pool.request_read(|| panic!("cancelled queued work must not start"))
        });
        finished_tx.send(result.map(|_| ())).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    parent.cancel();
    let error = finished_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap_err();
    worker.join().unwrap();
    assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
    assert_eq!(pool.usage().idle, 1);
    assert_eq!(pool.usage().opened, 1);
    drop(held);
    assert!(pool.try_admit_read().is_some());
}

#[test]
fn panic_error_and_cancelled_transaction_return_clean_capacity() {
    let (_directory, pool) = pool(1);
    let panic = catch_unwind(AssertUnwindSafe(|| pool.request_read(|| {
        pool.get().execute_batch("BEGIN").unwrap();
        panic!("request panic");
    })));
    assert!(panic.is_err());
    assert!(!pool.has_request_reader());
    assert!(pool.try_admit_read().is_some());
    let error: Result<()> = pool.request_read(|| anyhow::bail!("read failed")).unwrap();
    assert_eq!(error.unwrap_err().to_string(), "read failed");
    assert!(pool.try_admit_read().is_some());

    let budget = ReadBudget::new("/transaction", Duration::from_secs(30));
    let error = read_budget::with(Some(budget.clone()), || pool.request_read(|| {
        pool.get().execute_batch("BEGIN").unwrap();
        budget.cancel();
        read_budget::check()
    })).unwrap().unwrap_err();
    assert_eq!(error.code, "read-deadline");
    assert!(pool.try_admit_read().is_some());
    pool.request_read(|| assert!(pool.get().is_autocommit())).unwrap();
    assert_eq!(pool.usage().idle, 1);
}

#[test]
fn connection_open_failure_returns_capacity() {
    let (directory, pool) = pool(1);
    let raw = pool.get();
    std::fs::remove_file(directory.path().join("read.sqlite")).unwrap();
    let error = pool.request_read(|| panic!("failed acquisition must not run work")).unwrap_err();
    assert!(error.to_string().contains("open st read connection"));
    assert!(pool.try_admit_read().is_some());
    drop(raw);
}

#[test]
fn wrong_pool_permit_cannot_corrupt_either_pool() {
    let (_first_directory, first) = pool(1);
    let (_second_directory, second) = pool(1);
    let permit = first.try_admit_read().unwrap();
    let error = second.request_read_with_permit(permit, || panic!("wrong-pool work must not start")).unwrap_err();
    assert_eq!(error.to_string(), "read permit belongs to another pool");
    assert!(first.try_admit_read().is_some());
    assert!(second.try_admit_read().is_some());
    assert_eq!(first.usage().opened, 1);
    assert_eq!(second.usage().opened, 1);
}

#[test]
fn escaped_request_and_snapshot_loans_hold_capacity_until_the_connection_closes() {
    let (_directory, pool) = pool(1);
    let escaped = pool.request_read(|| pool.get()).unwrap();
    assert!(!pool.has_request_reader());
    assert_eq!((pool.usage().open, pool.usage().idle), (1, 0));
    assert!(pool.try_admit_read().is_none());
    drop(escaped);
    assert_eq!((pool.usage().open, pool.usage().idle), (0, 0));
    assert!(pool.try_admit_read().is_some());

    let store = with_read_limit_for_test(1, || Store::open_memory("escape", Arc::new(Plain))).unwrap();
    let escaped = store.readers.request_read(|| {
        store.read_snapshot(|_| Ok(store.readers.get()))
    }).unwrap().unwrap();
    assert!(!store.readers.has_request_reader());
    assert!(store.readers.try_admit_read().is_none());
    drop(escaped);
    assert_eq!((store.readers.usage().open, store.readers.usage().idle), (0, 0));
    assert!(store.readers.try_admit_read().is_some());
}

#[test]
fn legacy_raw_loans_remain_ungated_and_can_nest_at_one() {
    let (_directory, pool) = pool(1);
    let first = pool.get();
    let second = pool.try_get().unwrap();
    assert_eq!(pool.usage().open, 2);
    assert!(pool.try_admit_read().is_some());
    drop((first, second));
}

#[test]
fn read_worker_configuration_requires_a_supported_positive_integer() {
    for invalid in [None, Some(""), Some("bad"), Some("0"), Some("-1"), Some(" 32"), Some("999999999999999999999999999999999999")] {
        assert_eq!(configured_max_read_workers(invalid), MAX_READ_WORKERS);
    }
    assert_eq!(configured_max_read_workers(Some(&usize::MAX.to_string())), MAX_READ_WORKERS);
    assert_eq!(configured_max_read_workers(Some("1")), 1);
    assert_eq!(configured_max_read_workers(Some("32")), 32);
    assert_eq!(configured_max_read_workers(Some("64")), 64);
}

#[test]
fn constructor_override_is_nested_thread_local_and_unwind_safe() {
    let default = constructor_read_limit();
    with_read_limit_for_test(1, || {
        assert_eq!(constructor_read_limit(), 1);
        with_read_limit_for_test(32, || assert_eq!(constructor_read_limit(), 32));
        assert_eq!(constructor_read_limit(), 1);
        assert_eq!(std::thread::spawn(constructor_read_limit).join().unwrap(), default);
        assert!(catch_unwind(|| with_read_limit_for_test(2, || panic!("override panic"))).is_err());
        assert_eq!(constructor_read_limit(), 1);
    });
    assert_eq!(constructor_read_limit(), default);
}
