//! Admission of top-level writer-bound callbacks before they occupy blocking threads.
use super::*;
use std::future::Future;
use std::time::Instant;

pub(super) struct WorkerQueue {
    epoch: Instant,
    depth: AtomicU64,
    // u64::MAX means no busy period has been published yet.
    busy_since_micros: AtomicU64,
}

impl WorkerQueue {
    pub(super) fn new() -> Arc<Self> {
        let queue = Arc::new(Self {
            epoch: Instant::now(),
            depth: AtomicU64::new(0),
            busy_since_micros: AtomicU64::new(u64::MAX),
        });
        telemetry::register_worker_queue(&queue);
        queue
    }

    pub(super) fn sample(&self) -> (u64, f64) {
        (self.depth.load(Ordering::Acquire), self.busy_age())
    }

    pub(super) fn busy_age(&self) -> f64 {
        if self.depth.load(Ordering::Acquire) == 0 {
            return 0.0;
        }
        let since = self.busy_since_micros.load(Ordering::Acquire);
        // An enqueue may not have published its start yet. Epoch zero remains an
        // upper bound in that window, rather than hiding potentially old work.
        let since = if since == u64::MAX { 0 } else { since };
        let age = self.micros().saturating_sub(since);
        if self.depth.load(Ordering::Acquire) == 0 {
            return 0.0;
        }
        std::time::Duration::from_micros(age).as_secs_f64()
    }

    fn micros(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn enqueue(self: &Arc<Self>) -> QueuedWorker {
        std::sync::LazyLock::force(&telemetry::METRICS);
        let at = Instant::now();
        let micros = u64::try_from(at.duration_since(self.epoch).as_micros()).unwrap_or(u64::MAX);
        if self.depth.fetch_add(1, Ordering::AcqRel) == 0 {
            self.busy_since_micros.fetch_min(micros, Ordering::AcqRel);
        }
        QueuedWorker { queue: self.clone(), micros, at }
    }
}

struct QueuedWorker {
    queue: Arc<WorkerQueue>,
    micros: u64,
    at: Instant,
}

impl Drop for QueuedWorker {
    fn drop(&mut self) {
        let last = self.queue.depth.fetch_sub(1, Ordering::AcqRel) == 1;
        if last {
            let since = self.queue.busy_since_micros.swap(u64::MAX, Ordering::AcqRel);
            // A new 0->1 enqueue may publish before our clear. Recheck depth and
            // restore the earlier start with atomic min, never moving a newer
            // busy period forward. Overlapping empty transitions can retain an
            // older start: this is a conservative bound, not exact oldest age.
            if self.queue.depth.load(Ordering::Acquire) != 0 {
                self.queue.busy_since_micros.fetch_min(since.min(self.micros), Ordering::AcqRel);
            }
        }
    }
}

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
#[derive(Clone)]
pub struct WriteWorkers {
    admission: Arc<Semaphore>,
    inherited: Option<WritePermit>,
    queue: Arc<WorkerQueue>,
    requester: Option<Arc<tokio::sync::oneshot::Sender<()>>>,
    route: Option<opentelemetry::KeyValue>,
}

impl WriteWorkers {
    /// Observe a detached request without cancelling or skipping its work.
    /// `route` must be the router's normalized matched path, never a raw URI.
    pub fn with_requester(mut self, requester: Arc<tokio::sync::oneshot::Sender<()>>, route: String) -> Self {
        self.requester = Some(requester);
        self.route = Some(opentelemetry::KeyValue::new("origin", Arc::<str>::from(route)));
        self
    }

    /// Admit before submitting work; dependent callbacks retain their parent's slot.
    pub async fn spawn_blocking<F, T>(self, work: F) -> Result<T, tokio::task::JoinError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let queued = self.inherited.is_none().then(|| self.queue.enqueue());
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
        tokio::task::spawn_blocking(move || {
            if let Some(queued) = queued {
                let wait = queued.at.elapsed();
                drop(queued);
                telemetry::METRICS.worker_wait.record(
                    wait.as_secs_f64(),
                    &[self.route.unwrap_or_else(|| opentelemetry::KeyValue::new("origin", "other"))],
                );
            }
            if self.requester.is_some_and(|requester| requester.is_closed()) {
                telemetry::METRICS.abandoned_callback_starts.add(1, &[opentelemetry::KeyValue::new("outcome", "ran")]);
            }
            permit.run(work)
        }).await
    }
}

impl WriterConnection {
    /// Clone admission before moving the store into its callback. No store ownership is
    /// retained here; a dependent callback's inherited permit remains owned until it ends.
    pub fn write_workers(&self) -> WriteWorkers {
        WriteWorkers {
            admission: self.workers.clone(),
            inherited: self.current_write_permit(),
            queue: self.worker_queue.clone(),
            requester: None,
            route: None,
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
        let queue = self.worker_queue.clone();
        async move {
            let queued = queue.enqueue();
            let permit = admission
                .clone()
                .acquire_owned()
                .await
                .expect("writer worker admission is never closed");
            telemetry::METRICS.worker_wait.record(queued.at.elapsed().as_secs_f64(), &[opentelemetry::KeyValue::new("origin", "other")]);
            drop(queued);
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

    #[test]
    fn busy_period_covers_all_queued_callbacks_and_clears() {
        let queue = WorkerQueue::new();
        let first = queue.enqueue();
        let since = queue.busy_since_micros.load(Ordering::Relaxed);
        assert_ne!(since, u64::MAX);
        let second = queue.enqueue();
        assert_eq!(queue.busy_since_micros.load(Ordering::Relaxed), since);
        drop(first);
        assert_eq!(queue.busy_since_micros.load(Ordering::Relaxed), since);
        assert!(since <= second.micros);
        drop(second);
        assert_eq!(queue.busy_since_micros.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(queue.sample(), (0, 0.0));
        assert_eq!(queue.busy_age(), 0.0);
        let next = queue.enqueue();
        assert_ne!(queue.busy_since_micros.load(Ordering::Relaxed), u64::MAX);
        drop(next);
        assert_eq!(queue.busy_age(), 0.0);
    }

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
