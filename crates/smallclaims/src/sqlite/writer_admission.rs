//! Admission of top-level writer-bound callbacks before they occupy blocking threads.
use super::*;
use std::future::Future;

pub const MAX_WRITE_WORKERS: usize = 32;

pub fn max_write_workers() -> usize {
    static MAX: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        configured_max_write_workers(
            std::env::var("SMALLCLAIMS_MAX_WRITE_WORKERS")
                .ok()
                .as_deref(),
        )
    });
    *MAX
}

fn configured_max_write_workers(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=Semaphore::MAX_PERMITS).contains(value))
        .unwrap_or(MAX_WRITE_WORKERS)
}

thread_local! {
    static WORKERS: RefCell<Vec<WritePermit>> = const { RefCell::new(Vec::new()) };
    #[cfg(any(test, feature = "test-support"))]
    static LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub fn with_write_limit_for_test<T>(limit: usize, work: impl FnOnce() -> T) -> T {
    assert!((1..=Semaphore::MAX_PERMITS).contains(&limit));
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LIMIT.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(LIMIT.with(|slot| slot.replace(Some(limit))));
    work()
}

pub(super) fn constructor_write_limit() -> usize {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(limit) = LIMIT.with(std::cell::Cell::get) {
        return limit;
    }
    max_write_workers()
}

/// Capacity for one top-level callback, retained by any dependent nested callback.
/// This admits work; it does not acquire SQLite's writer or change its loan class.
#[derive(Clone)]
pub struct WritePermit {
    admission: Arc<Semaphore>,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl WritePermit {
    /// Install the worker's admission context on this thread for dependent callbacks.
    /// The permit remains held through success, panic and detached blocking completion.
    pub fn run<T>(self, work: impl FnOnce() -> T) -> T {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                WORKERS.with(|workers| {
                    workers.borrow_mut().pop();
                });
            }
        }
        WORKERS.with(|workers| workers.borrow_mut().push(self));
        let _restore = Restore;
        work()
    }
}

/// An owned admission handle, so a store can move into its own blocking callback.
pub struct WriteWorkers {
    admission: Arc<Semaphore>,
    inherited: Option<WritePermit>,
}

impl WriteWorkers {
    /// Admit before submitting work; dependent callbacks retain their parent's slot.
    pub async fn spawn_blocking<F, T>(self, work: F) -> Result<T, tokio::task::JoinError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let permit = match self.inherited {
            Some(permit) => permit,
            None => {
                let permit = self
                    .admission
                    .clone()
                    .acquire_owned()
                    .await
                    .expect("writer worker admission is never closed");
                WritePermit {
                    admission: self.admission,
                    _permit: Arc::new(permit),
                }
            }
        };
        tokio::task::spawn_blocking(move || permit.run(work)).await
    }
}

impl WriterConnection {
    /// Clone admission before moving the store into its callback. No store ownership is
    /// retained here; a dependent callback's inherited permit remains owned until it ends.
    pub fn write_workers(&self) -> WriteWorkers {
        WriteWorkers {
            admission: self.workers.clone(),
            inherited: self.current_write_permit(),
        }
    }

    /// The current same-writer callback's capacity, for a dependent callback that moves
    /// to another blocking thread. Never carry this into independent or detached tasks.
    pub fn current_write_permit(&self) -> Option<WritePermit> {
        WORKERS.with(|workers| {
            workers
                .borrow()
                .iter()
                .rev()
                .find(|permit| Arc::ptr_eq(&permit.admission, &self.workers))
                .cloned()
        })
    }

    /// Wait asynchronously before spawning a top-level writer-bound callback. Tokio's
    /// semaphore grants queued admission in FIFO order. No store locks may be held here.
    pub fn admit_write(&self) -> impl Future<Output = WritePermit> + Send + use<> {
        let admission = self.workers.clone();
        async move {
            let permit = admission
                .clone()
                .acquire_owned()
                .await
                .expect("writer worker admission is never closed");
            WritePermit {
                admission,
                _permit: Arc::new(permit),
            }
        }
    }

    /// Run writer-bound work with asynchronous admission before entering the blocking pool.
    /// Call from outside store locks/read snapshots. A same-thread dependent callback shares
    /// its parent's permit instead of waiting behind capacity its parent already holds.
    /// Dropping pending admission submits nothing; started work retains capacity until it ends.
    pub fn spawn_blocking<F, T>(
        &self,
        work: F,
    ) -> impl Future<Output = Result<T, tokio::task::JoinError>> + Send + use<F, T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.write_workers().spawn_blocking(work)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use std::task::Poll;
    use std::time::Duration;

    fn writer(limit: usize) -> Arc<WriterConnection> {
        with_write_limit_for_test(limit, || {
            Arc::new(WriterConnection::new(
                Connection::open_in_memory().unwrap(),
                Arc::new(AtomicU64::new(0)),
            ))
        })
    }

    #[test]
    fn invalid_limits_use_the_documented_default() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("bad"),
            Some("-1"),
            Some("18446744073709551615"),
        ] {
            assert_eq!(configured_max_write_workers(value), MAX_WRITE_WORKERS);
        }
        assert_eq!(configured_max_write_workers(Some("1")), 1);
        assert_eq!(configured_max_write_workers(Some("64")), 64);
    }

    #[test]
    fn queued_128_writes_start_only_the_admitted_blocking_threads() {
        const LIMIT: usize = 32;
        let writer = writer(LIMIT);
        let hold = writer.write();
        let started = Arc::new(AtomicUsize::new(0));
        let counted = started.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .on_thread_start(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
            let mut jobs: Vec<_> = (0..128)
                .map(|_| {
                    let writing = writer.clone();
                    let entered = entered.clone();
                    Box::pin(writer.spawn_blocking(move || {
                        entered.send(std::thread::current().id()).unwrap();
                        drop(writing.write());
                    }))
                })
                .collect();
            // Poll every caller: 32 enter the pool, 96 queue only on asynchronous admission.
            for job in &mut jobs {
                assert!(matches!(poll!(job.as_mut()), Poll::Pending));
            }
            let observed = tokio::time::timeout(Duration::from_secs(5), async {
                let mut threads = std::collections::HashSet::new();
                for _ in 0..LIMIT {
                    threads.insert(entries.recv().await.unwrap());
                }
                threads
            })
            .await;
            let thread_count = started.load(Ordering::SeqCst);
            let extra = entries.try_recv();
            drop(hold); // Release even if the assertions below fail.
            for job in jobs {
                job.await.unwrap();
            }
            assert_eq!(observed.unwrap().len(), LIMIT);
            assert!(extra.is_err(), "unadmitted callbacks started");
            assert!(
                thread_count <= LIMIT + 2,
                "128 queued writers made {thread_count} runtime threads"
            );
        });
    }

    #[tokio::test]
    async fn admission_is_fifo_and_cancelling_a_waiter_releases_its_place() {
        let writer = writer(1);
        let held = writer.admit_write().await;
        let mut first = Box::pin(writer.admit_write());
        let mut cancelled = Box::pin(writer.admit_write());
        let mut last = Box::pin(writer.admit_write());
        assert!(poll!(first.as_mut()).is_pending());
        assert!(poll!(cancelled.as_mut()).is_pending());
        assert!(poll!(last.as_mut()).is_pending());
        drop(cancelled);
        drop(held);
        assert!(poll!(last.as_mut()).is_pending());
        drop(first.await);
        drop(last.await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn nested_callback_reuses_capacity_and_panic_returns_it() {
        let writer = writer(1);
        let inner = writer.clone();
        let nested = writer.spawn_blocking(move || {
            let nested_writer = inner.clone();
            tokio::runtime::Handle::current()
                .block_on(inner.spawn_blocking(move || {
                    drop(nested_writer.write());
                    42
                }))
                .unwrap()
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), nested)
                .await
                .unwrap()
                .unwrap(),
            42
        );
        assert!(
            writer
                .spawn_blocking(|| panic!("worker panic"))
                .await
                .unwrap_err()
                .is_panic()
        );
        assert_eq!(writer.spawn_blocking(|| 7).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn a_detached_started_worker_keeps_its_capacity() {
        let writer = writer(1);
        let (entered, occupied) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let mut work = Box::pin(writer.spawn_blocking(move || {
            entered.send(()).unwrap();
            released.recv().unwrap();
        }));
        assert!(poll!(work.as_mut()).is_pending());
        occupied.await.unwrap();
        drop(work);
        let mut next = Box::pin(writer.admit_write());
        assert!(poll!(next.as_mut()).is_pending());
        release.send(()).unwrap();
        drop(
            tokio::time::timeout(Duration::from_secs(5), next)
                .await
                .unwrap(),
        );
    }
}
