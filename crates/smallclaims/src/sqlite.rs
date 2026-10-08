//! The store's SQLite connections: one writer thread that batches writes, and a pool of read
//! connections that a thread can pin to one snapshot.

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, Transaction};

mod transaction_finalizer;
use transaction_finalizer::TransactionFinalizers;
pub use transaction_finalizer::WriterTransaction;

pub(crate) mod writer_observer;
pub use writer_observer::WriterObserver;

use crate::store::current_index;

/// Read connections a store keeps between reads; more open while more reads run at once.
/// Each requests `read_cache_kib()` KiB of pages. Opening a connection also parses the database schema
/// (`sqlite3Init`), which under the daemon's read pattern cost more CPU than the reads
/// themselves (issue #946: 55% of daemon CPU was `ReadPool::get` reopening connections the
/// pool had just closed), so the pool keeps every connection it opened, up to this many.
pub const MAX_IDLE_READ_CONNECTIONS: usize = 128;

/// A fixed page-cache target per reader, leaving retained schema and statement caches intact.
/// SQLite's page-cache target excludes statements, schema, query results and allocator overhead.
pub const READ_CACHE_KIB: usize = 2048;
pub const WRITE_CACHE_KIB: usize = 32768;

pub fn read_cache_kib() -> usize {
    static CACHE: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        configured_read_cache_kib(std::env::var("SMALLCLAIMS_READ_CACHE_KIB").ok().as_deref())
    });
    *CACHE
}

fn configured_read_cache_kib(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=i32::MAX as usize).contains(value))
        .unwrap_or(READ_CACHE_KIB)
}

/// Idle retention ceiling, not a limit on concurrent reads or open connections.
/// `MAX_IDLE_READ_CONNECTIONS`, or `SMALLCLAIMS_MAX_IDLE_READ_CONNECTIONS` when that parses.
pub fn max_idle_read_connections() -> usize {
    static MAX: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("SMALLCLAIMS_MAX_IDLE_READ_CONNECTIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(MAX_IDLE_READ_CONNECTIONS)
    });
    *MAX
}

/// Prepared statements each connection keeps. The default of 16 is fewer than the cached
/// statements one status reduction alone runs, so they evicted each other and were planned anew
/// for every subject.
pub const STATEMENT_CACHE_CAPACITY: usize = 128;

/// Owns one synchronous committed-write callback. Dropping it unregisters the callback and
/// waits for an invocation already running on another thread to finish. Dropping it from its
/// own callback disables future invocations without waiting for itself. It holds only weak
/// references, so keeping it after the store closes neither keeps the writer alive nor retains
/// the callback's captures.
#[must_use = "dropping the handle unregisters the committed-write observer"]
pub struct CommitObserver {
    observers: Weak<CommitObservers>,
    callback: Weak<CommitObserverCallback>,
}

struct CommitObserverCallback {
    state: Mutex<CommitObserverState>,
    completed: Condvar,
}

type CommitCallback = Arc<dyn Fn(&Connection) + Send + Sync>;

struct CommitObserverState {
    run: Option<CommitCallback>,
    running: Option<std::thread::ThreadId>,
}

/// Always release waiters, including when a callback panics. The writer serializes invocations,
/// but an observer can be dropped concurrently or by the callback's final upgraded weak owner.
struct CommitObserverInvocation<'a> {
    callback: &'a CommitObserverCallback,
}

impl Drop for CommitObserverInvocation<'_> {
    fn drop(&mut self) {
        let mut state = self
            .callback
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.running = None;
        if state.run.is_none() {
            self.callback.completed.notify_all();
        }
    }
}

#[derive(Default)]
struct CommitObservers {
    active: AtomicBool,
    /// Commits clone this Arc, not the vector. Registration/removal copy the vector only while
    /// a commit is using its previous snapshot, and never hold this lock while invoking callbacks.
    callbacks: Mutex<Arc<Vec<Arc<CommitObserverCallback>>>>,
}

impl CommitObservers {
    fn register(
        self: &Arc<Self>,
        callback: impl Fn(&Connection) + Send + Sync + 'static,
    ) -> CommitObserver {
        let callback = Arc::new(CommitObserverCallback {
            state: Mutex::new(CommitObserverState {
                run: Some(Arc::new(callback)),
                running: None,
            }),
            completed: Condvar::new(),
        });
        let mut callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::make_mut(&mut callbacks).push(callback.clone());
        self.active.store(true, Ordering::Release);
        CommitObserver {
            observers: Arc::downgrade(self),
            callback: Arc::downgrade(&callback),
        }
    }

    fn notify(&self, connection: &Connection) {
        // The common case takes no lock and allocates nothing.
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let callbacks = self
            .callbacks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let current = std::thread::current().id();
        for callback in callbacks.iter() {
            let run = {
                let mut state = callback
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let Some(run) = state.run.clone() else {
                    continue;
                };
                debug_assert!(state.running.is_none(), "the writer serializes observers");
                state.running = Some(current);
                run
            };
            let _invocation = CommitObserverInvocation { callback };
            run(connection);
        }
    }
}

impl Drop for CommitObserver {
    fn drop(&mut self) {
        let Some(callback) = self.callback.upgrade() else {
            return;
        };
        // Deactivate before removing it from future snapshots; an in-flight snapshot must not
        // start it after drop returns. Wait for another thread, but not for ourselves: dropping
        // the callback's last upgraded weak owner can implicitly drop this handle.
        let removed = {
            let mut state = callback
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let removed = state.run.take();
            if state
                .running
                .is_some_and(|running| running != std::thread::current().id())
            {
                while state.running.is_some() {
                    state = callback
                        .completed
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            removed
        };
        // Release captures outside the state and registry locks.
        drop(removed);
        if let Some(observers) = self.observers.upgrade() {
            let mut callbacks = observers
                .callbacks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Arc::make_mut(&mut callbacks).retain(|entry| !Arc::ptr_eq(entry, &callback));
            observers
                .active
                .store(!callbacks.is_empty(), Ordering::Release);
        }
    }
}

/// Recycle a fully backfilled WAL using a dedicated checkpoint connection, never the writer.
/// PASSIVE does page copying without taking the writer lock. TRUNCATE is attempted only after
/// that copy completes, with no busy wait: an active reader or writer defers recycling.
pub fn checkpoint_idle_wal(connection: &Connection) -> Result<bool> {
    if !checkpoint_wal_backfilled(connection)? {
        return Ok(false);
    }
    truncate_idle_wal(connection)
}

/// Copy pages without taking SQLite's writer lock. A live Store must serialize the later
/// TRUNCATE with its writer queue; a zero busy timeout alone does not exclude a checkpoint.
pub fn checkpoint_wal_backfilled(connection: &Connection) -> Result<bool> {
    let report = checkpoint_wal_report(connection)?;
    Ok(report.frames >= 0 && report.frames == report.backfilled)
}

#[derive(Debug)]
pub struct WalCheckpointReport {
    pub frames: i32,
    pub backfilled: i32,
    pub passive_ms: u128,
    pub writer_wait_ms: u128,
    pub truncate_ms: Option<u128>,
    pub recycled: bool,
}

pub fn checkpoint_wal_report(connection: &Connection) -> Result<WalCheckpointReport> {
    connection.busy_timeout(std::time::Duration::ZERO)?;
    let started = std::time::Instant::now();
    let (_, frames, backfilled): (i32, i32, i32) =
        connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    Ok(WalCheckpointReport {
        frames, backfilled, passive_ms: started.elapsed().as_millis(),
        writer_wait_ms: 0, truncate_ms: None, recycled: false,
    })
}

/// Attempt recycling without waiting for readers. TRUNCATE takes SQLite's writer lock;
/// the daemon calls this only while it has borrowed the Store's sole writer from its queue.
pub fn truncate_idle_wal(connection: &Connection) -> Result<bool> {
    connection.busy_timeout(std::time::Duration::ZERO)?;
    let busy: i32 = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    Ok(busy == 0)
}

/// The store's only write connection, owned by one writer thread. Writes queue in front of it in
/// arrival order: a batched write runs on the writer thread with the others queued behind it, each
/// in a savepoint of one transaction that commits once for all of them, and its caller hears back
/// after that commit. `write` lends the connection itself to its caller until the guard drops,
/// for writes that manage their own transactions. Nothing else ever takes SQLite's write lock.
pub struct WriterConnection {
    pub jobs: Mutex<Option<std::sync::mpsc::Sender<WriterJob>>>,
    pub thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub committed_index: Arc<AtomicU64>,
    observers: Arc<CommitObservers>,
    finalizers: Arc<TransactionFinalizers>,
    mutation_observer: Option<Arc<writer_observer::MutationState>>,
    /// Transactions the writer committed for batched writes, and the batched writes in them.
    /// Tests read them; `st replication status` counts every commit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub batches: Arc<(AtomicU64, AtomicU64)>,
}

pub enum WriterJob {
    /// Runs in a savepoint of the writer's next transaction and says whether it succeeded. `run`
    /// is declared first, so it drops before `done` wakes its caller.
    Batched {
        run: Box<dyn FnOnce(&Transaction<'_>) -> bool + Send>,
        profile: Option<crate::profile::Op>,
        /// When profiling, when its caller began to wait and who held the writer then.
        wait: Option<crate::profile::WriterWait>,
        done: std::sync::mpsc::SyncSender<Result<(), String>>,
    },
    /// Hands the connection to a caller until its guard gives it back.
    Lend {
        lent: std::sync::mpsc::SyncSender<Connection>,
        returned: std::sync::mpsc::Receiver<Connection>,
    },
}

/// At most this many batched writes share one transaction.
pub const WRITE_BATCH_LIMIT: usize = 256;

/// A batch takes in more queued writes only while it has run for less than this, so a write that
/// arrives during a busy moment waits for about this long plus one write, not for a long batch.
pub const WRITE_BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);

pub struct WriterGuard<'a> {
    pub connection: Option<Connection>,
    pub give_back: std::sync::mpsc::SyncSender<Connection>,
    pub committed_index: &'a AtomicU64,
    observers: &'a CommitObservers,
    finalizers: &'a Arc<TransactionFinalizers>,
    mutation_observer: Option<&'a Arc<writer_observer::MutationState>>,
    /// When profiling, when this thread took the writer.
    pub acquired: Option<std::time::Instant>,
    /// The connection's changed-row count when it was lent, so the rows this thread changed are
    /// noted for it when it gives the connection back; see `touched::writes`.
    pub changes_at_lend: u64,
}

impl WriterConnection {
    pub fn new(connection: Connection, committed_index: Arc<AtomicU64>) -> Self {
        Self::new_inner(connection, committed_index, None)
            .expect("writer without an observer needs no schema check")
    }

    /// Install a row observer before handing the connection to the writer queue. Its callbacks
    /// own the update/authorizer hooks and resolve committed state before acknowledgements.
    /// Observers must not write, borrow the writer, panic, or retain a strong owner cycle.
    pub fn new_with_observer(
        connection: Connection,
        committed_index: Arc<AtomicU64>,
        observer: Arc<dyn WriterObserver>,
    ) -> Result<Self> {
        Self::new_inner(connection, committed_index, Some(observer))
    }

    fn new_inner(
        connection: Connection,
        committed_index: Arc<AtomicU64>,
        observer: Option<Arc<dyn WriterObserver>>,
    ) -> Result<Self> {
        let mutation_observer = observer
            .map(|observer| writer_observer::MutationState::new(&connection, observer))
            .transpose()?;
        let mutations = mutation_observer.clone();
        let (jobs, queue) = std::sync::mpsc::channel::<WriterJob>();
        let index = committed_index.clone();
        let batches = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let counted = batches.clone();
        let observers = Arc::new(CommitObservers::default());
        let observed = observers.clone();
        let finalizers = Arc::new(TransactionFinalizers::default());
        let finalized = finalizers.clone();
        let thread = std::thread::Builder::new()
            .name("st3-writer".into())
            .spawn(move || {
                write_queue(
                    connection,
                    queue,
                    &index,
                    &counted,
                    (&observed, &finalized),
                    mutations.as_ref(),
                )
            })
            .expect("the writer thread starts");
        Ok(Self {
            jobs: Mutex::new(Some(jobs)),
            thread: Mutex::new(Some(thread)),
            committed_index,
            observers,
            finalizers,
            mutation_observer,
            batches,
        })
    }

    /// Observe every successfully committed batch and every returned lent writer, synchronously
    /// after its committed index is updated and before its write can acknowledge success.
    /// A callback may read authority from this same connection, but must not write or acquire the
    /// writer. Dropping its own observer handle is safe and disables future invocations.
    /// Batched callbacks run on the writer thread; lent callbacks run on the returning thread.
    /// Lent callers must finish their transactions and drop the guard before acknowledging a
    /// write, since observers run on guard return rather than on each explicit SQL commit.
    /// Callbacks must fail closed on read errors themselves and must not panic: a panic is not
    /// swallowed, prevents success acknowledgement, and stops the writer.
    ///
    /// Registration does not acquire the writer or invoke the callback. A commit already taking
    /// its callback snapshot may miss a concurrent registration, so register first, then recheck
    /// current authority outside any pinned read snapshot before exposing an authorization.
    /// Capture weak references to the store or other owners to avoid ownership cycles.
    pub fn observe_commits(
        &self,
        callback: impl Fn(&Connection) + Send + Sync + 'static,
    ) -> CommitObserver {
        self.observers.register(callback)
    }

    /// Install exactly one transaction-owned adapter, explicitly and before attesting source
    /// coverage. Acquiring the writer finishes all earlier queued/lent work before installation.
    /// The callback runs once before each managed outer commit, with source and callback writes
    /// in that same transaction. An error rolls back the outer transaction and propagates.
    ///
    /// Only batched writes and WriterGuard::transaction[_with_behavior] are covered. Raw
    /// Connection transactions, autocommit and SQL BEGIN/COMMIT bypass this boundary and need
    /// independent source capture/fencing. Registration is not a bootstrap or coverage proof.
    /// The callback must bound its work, must not end the transaction, acquire the writer, or
    /// retain a strong Store ownership cycle. It remains installed for this writer's lifetime.
    pub fn install_transaction_finalizer(
        &self,
        callback: impl Fn(&Transaction<'_>) -> Result<()> + Send + Sync + 'static,
    ) -> Result<()> {
        self.install_transaction_hooks(|_| Ok(()), callback)
    }

    /// Install one paired adapter. Prepare runs INSIDE the newly begun outer transaction
    /// before any managed source job/helper; finalize runs before its commit. An adapter can
    /// reject/fence preexisting raw capture and set/clear its transaction-owned scope marker.
    /// Both phases share source rollback; failure/panic does not acknowledge a successful write.
    /// The same coverage, work bounds and lifetime restrictions as the finalizer apply.
    pub fn install_transaction_hooks(
        &self,
        prepare: impl Fn(&Transaction<'_>) -> Result<()> + Send + Sync + 'static,
        finalize: impl Fn(&Transaction<'_>) -> Result<()> + Send + Sync + 'static,
    ) -> Result<()> {
        let _writer = self.write();
        self.finalizers.install(prepare, finalize)
    }

    pub fn send(&self, job: WriterJob) {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .expect("the writer queue is open while the store is")
            .send(job)
            .expect("the writer thread runs while the store is open");
    }

    /// The writer connection itself, lent until the guard drops, after every write queued before
    /// this one. A panic while it is lent rolls back the open transaction as it unwinds and still
    /// gives the connection back, so it cannot disable the store.
    pub fn write(&self) -> WriterGuard<'_> {
        debug_assert_no_pinned_read();
        let wait = crate::profile::writer_waiting();
        let (lent, lent_here) = std::sync::mpsc::sync_channel(1);
        let (give_back, returned) = std::sync::mpsc::sync_channel(1);
        self.send(WriterJob::Lend { lent, returned });
        let connection = lent_here
            .recv()
            .expect("the writer thread lends its connection");
        WriterGuard {
            changes_at_lend: connection.total_changes(),
            connection: Some(connection),
            give_back,
            committed_index: &self.committed_index,
            observers: &self.observers,
            finalizers: &self.finalizers,
            mutation_observer: self.mutation_observer.as_ref(),
            acquired: crate::profile::writer_acquired(wait),
        }
    }

    /// Run `job` in a savepoint of the writer's next transaction, together with the other writes
    /// queued with it, and answer once that transaction commits. An `Err` from `job` rolls back
    /// only its own savepoint. The outer `Err` is a commit that failed, which undid every write in
    /// the batch.
    pub fn batched<'job, T: Send + 'job, E: Send + 'job>(
        &self,
        job: impl FnOnce(&Transaction<'_>) -> std::result::Result<T, E> + Send + 'job,
    ) -> std::result::Result<std::result::Result<T, E>, String> {
        debug_assert_no_pinned_read();
        let outcome = Mutex::new(None);
        let slot = &outcome;
        let changed_rows = AtomicU64::new(0);
        let changed = &changed_rows;
        // The job runs on the writer thread; what it wrote is handed back to this one.
        let capture = crate::touched::recording_wrote();
        let wrote_rows = Mutex::new(Vec::new());
        let wrote = &wrote_rows;
        let run: Box<dyn FnOnce(&Transaction<'_>) -> bool + Send + '_> = Box::new(move |tx| {
            let before = tx.total_changes();
            let (result, written) = if capture {
                crate::touched::record_wrote(|| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(tx)))
                })
            } else {
                (
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(tx))),
                    Vec::new(),
                )
            };
            let succeeded = matches!(result, Ok(Ok(_)));
            if succeeded {
                changed.store(tx.total_changes().saturating_sub(before), Ordering::Relaxed);
                *wrote.lock().unwrap_or_else(PoisonError::into_inner) = written;
            }
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
            succeeded
        });
        // SAFETY: `run` borrows `outcome` and whatever `job` borrows. This caller blocks below
        // until the writer thread has run `run` and committed, or has dropped it: `run` drops
        // before `done`, and `recv` returns only once `done` is sent or dropped. So nothing `run`
        // borrows is used after this frame returns.
        let run: Box<dyn FnOnce(&Transaction<'_>) -> bool + Send + 'static> =
            unsafe { std::mem::transmute(run) };
        let (done, done_here) = std::sync::mpsc::sync_channel(1);
        self.send(WriterJob::Batched {
            run,
            profile: crate::profile::current(),
            wait: crate::profile::writer_waiting(),
            done,
        });
        let committed = done_here
            .recv()
            .unwrap_or_else(|_| Err("the writer thread stopped".into()));
        let result = outcome.into_inner().unwrap_or_else(PoisonError::into_inner);
        match (result, committed) {
            (Some(Err(panic)), _) => std::panic::resume_unwind(panic),
            (Some(Ok(result)), Ok(())) => {
                crate::touched::note_writes(changed_rows.load(Ordering::Relaxed));
                for entry in wrote_rows
                    .into_inner()
                    .unwrap_or_else(PoisonError::into_inner)
                {
                    crate::touched::note_wrote(|| entry);
                }
                Ok(result)
            }
            // The batch failed to begin or to commit, or failed before it ran this write.
            (_, Err(error)) => Err(error),
            (None, Ok(())) => Err("the writer answered a write it did not run".into()),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn lock(&self) -> Result<WriterGuard<'_>, &'static str> {
        Ok(self.write())
    }
}

impl Drop for WriterConnection {
    fn drop(&mut self) {
        // Closing the queue ends the writer thread, which closes the connection.
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            // An observer may temporarily upgrade a weak store reference and release its last
            // owner on the writer thread. Closing its queue is enough there; never join itself.
            if thread.thread().id() != std::thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

/// The writer thread: run each batched write with the others queued behind it in one
/// transaction, and lend the connection to each lending write in its turn.
fn write_queue(
    mut connection: Connection,
    queue: std::sync::mpsc::Receiver<WriterJob>,
    committed_index: &AtomicU64,
    batches: &(AtomicU64, AtomicU64),
    callbacks: (&CommitObservers, &TransactionFinalizers),
    mutation_observer: Option<&Arc<writer_observer::MutationState>>,
) {
    let mut next = None;
    loop {
        let job = match next.take() {
            Some(job) => job,
            None => match queue.recv() {
                Ok(job) => job,
                // The store closed its queue.
                Err(_) => return,
            },
        };
        match job {
            WriterJob::Lend { lent, returned } => {
                if let Err(std::sync::mpsc::SendError(back)) = lent.send(connection) {
                    connection = back;
                    continue;
                }
                match returned.recv() {
                    Ok(back) => connection = back,
                    Err(_) => return,
                }
            }
            batched => {
                next = run_write_batch(
                    &mut connection,
                    batched,
                    &queue,
                    committed_index,
                    batches,
                    callbacks,
                    mutation_observer,
                );
            }
        }
    }
}

/// Run `first` and the batched writes queued behind it in one transaction, each in its own
/// savepoint, then commit once and answer every caller. The batch stops taking writes when it
/// reaches `WRITE_BATCH_LIMIT`, has run for `WRITE_BATCH_WINDOW`, fails, or meets a lending write,
/// which it returns to run next.
fn run_write_batch(
    connection: &mut Connection,
    first: WriterJob,
    queue: &std::sync::mpsc::Receiver<WriterJob>,
    committed_index: &AtomicU64,
    batches: &(AtomicU64, AtomicU64),
    callbacks: (&CommitObservers, &TransactionFinalizers),
    mutation_observer: Option<&Arc<writer_observer::MutationState>>,
) -> Option<WriterJob> {
    let (observers, finalizers) = callbacks;
    let hooks = finalizers.snapshot();
    let started = std::time::Instant::now();
    let mut answers = Vec::new();
    let mut lend = None;
    let (transaction, mut failure) =
        match connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
            Ok(transaction) => (Some(transaction), None),
            Err(error) => (None, Some(anyhow::Error::from(error))),
        };
    if let (Some(transaction), Some(hooks)) = (&transaction, hooks) {
        failure = hooks.prepare(transaction).err();
    }
    let mut job = Some(first);
    while let Some(current) = job.take() {
        let WriterJob::Batched {
            run,
            profile,
            wait,
            done,
        } = current
        else {
            lend = Some(current);
            break;
        };
        match (&transaction, &failure) {
            (Some(transaction), None) => {
                // The caller's operation waited until now, holds the writer while its write
                // runs, and shares the commit below with the rest of the batch.
                let _entered = crate::profile::enter(profile.as_ref());
                let acquired = crate::profile::writer_acquired(wait);
                let savepoint = (|| {
                    transaction.execute_batch("SAVEPOINT batched_write")?;
                    if run(transaction) {
                        transaction.execute_batch("RELEASE batched_write")
                    } else {
                        transaction
                            .execute_batch("ROLLBACK TO batched_write; RELEASE batched_write")
                    }
                })();
                crate::profile::writer_released(acquired);
                failure = savepoint.err().map(anyhow::Error::from);
            }
            // A batch that failed to begin answers its first write with the error.
            _ => drop(run),
        }
        answers.push(done);
        if failure.is_none()
            && answers.len() < WRITE_BATCH_LIMIT
            && started.elapsed() < WRITE_BATCH_WINDOW
        {
            job = queue.try_recv().ok();
        }
    }
    batches.0.fetch_add(1, Ordering::Relaxed);
    batches.1.fetch_add(answers.len() as u64, Ordering::Relaxed);
    let committed = match (transaction, failure) {
        (Some(transaction), None) => {
            let finalized = match hooks {
                Some(hooks) => hooks.run(&transaction),
                None => Ok(()),
            };
            finalized.and_then(|()| transaction.commit().map_err(anyhow::Error::from))
        }
        // Dropping the transaction rolls back every write in the batch.
        (_, Some(error)) => Err(error),
        (None, None) => unreachable!("a batch without a transaction failed to begin"),
    };
    if let Ok(index) = current_index(connection) {
        committed_index.store(index, Ordering::Release);
    }
    if let Some(observer) = mutation_observer {
        observer.resolved(connection);
    }
    if committed.is_ok() {
        observers.notify(connection);
    }
    let committed = committed.map_err(|error| format!("{error:#}"));
    for done in answers {
        let _ = done.send(committed.clone());
    }
    lend
}

impl WriterGuard<'_> {
    /// Begin a managed transaction and run prepare before returning it. Existing helpers
    /// accepting &Transaction can use the wrapper through dereference; commit finalizes
    /// before COMMIT. Prepare failure rolls back and retains its original error cause.
    pub fn transaction(&mut self) -> Result<WriterTransaction<'_>> {
        let hooks = self.finalizers.snapshot();
        let connection = self
            .connection
            .as_mut()
            .context("writer guard has no connection")?;
        let transaction = connection.transaction()?;
        if let Some(hooks) = hooks {
            hooks.prepare(&transaction)?;
        }
        Ok(WriterTransaction::new(transaction, hooks))
    }

    pub fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> Result<WriterTransaction<'_>> {
        let hooks = self.finalizers.snapshot();
        let connection = self
            .connection
            .as_mut()
            .context("writer guard has no connection")?;
        let transaction = connection.transaction_with_behavior(behavior)?;
        if let Some(hooks) = hooks {
            hooks.prepare(&transaction)?;
        }
        Ok(WriterTransaction::new(transaction, hooks))
    }
}

impl Deref for WriterGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("a writer guard holds the connection until it drops")
    }
}

impl DerefMut for WriterGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("a writer guard holds the connection until it drops")
    }
}

impl Drop for WriterGuard<'_> {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        if let Ok(index) = current_index(&connection) {
            self.committed_index.store(index, Ordering::Release);
        }
        if let Some(observer) = self.mutation_observer {
            observer.resolved(&connection);
        }
        self.observers.notify(&connection);
        crate::touched::note_writes(
            connection
                .total_changes()
                .saturating_sub(self.changes_at_lend),
        );
        crate::profile::writer_released(self.acquired.take());
        let _ = self.give_back.send(connection);
    }
}

/// Read connections. A read takes an idle connection, or opens another when every one is busy,
/// so a read never waits for another read to finish: the pool holds as many connections as reads
/// ever ran at once, retains up to `max_idle_read_connections()` between reads, and closes
/// excess idle connections. Reads see the last committed state and, in WAL mode, never wait
/// for the writer. The retention ceiling does not bound concurrent connections.
pub struct ReadPool {
    pub idle: Mutex<Vec<ReadConnection>>,
    /// Wakes a read waiting for an idle connection, which happens only when the operating system
    /// refuses another one, for example past the open file limit.
    pub returned: Condvar,
    pub path: PathBuf,
    pub shared_memory: bool,
    counts: Arc<ReaderCounts>,
}

#[derive(Default)]
struct ReaderCounts {
    open: AtomicUsize,
    peak: AtomicUsize,
    opened: AtomicU64,
}

/// Counts the actual connection lifetime, including a connection shared by a pinned snapshot.
pub struct ReadConnection {
    connection: Connection,
    counts: Arc<ReaderCounts>,
}

impl Deref for ReadConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl Drop for ReadConnection {
    fn drop(&mut self) {
        self.counts.open.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ReaderUsage {
    pub open: usize,
    pub idle: usize,
    pub peak: usize,
    pub opened: u64,
}

/// Reads checked out right now, so a pinned WAL can be traced to its holder. SQLite keeps no
/// list of who holds a snapshot, and the profile only records a read after it ends.
static LIVE_READS: Mutex<std::collections::BTreeMap<u64, LiveRead>> =
    Mutex::new(std::collections::BTreeMap::new());
static NEXT_LIVE_READ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct LiveRead {
    started: std::time::Instant,
    at: &'static std::panic::Location<'static>,
    /// A `Store::read_snapshot`: one read transaction held open for the whole closure. Any other
    /// checkout is a pooled connection lent out, which pins only while a statement is mid-step.
    snapshot: bool,
}

/// Ends the live-read entry on every exit path.
pub struct LiveReadToken(u64);

impl Drop for LiveReadToken {
    fn drop(&mut self) {
        LIVE_READS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Note a read starting at the caller's location.
#[track_caller]
pub fn register_live_read(snapshot: bool) -> LiveReadToken {
    let id = NEXT_LIVE_READ.fetch_add(1, Ordering::Relaxed);
    LIVE_READS.lock().unwrap_or_else(PoisonError::into_inner).insert(
        id,
        LiveRead {
            started: std::time::Instant::now(),
            at: std::panic::Location::caller(),
            snapshot,
        },
    );
    LiveReadToken(id)
}

/// The longest-running read checked out now, and how many there are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldestLiveRead {
    pub age_ms: u128,
    pub snapshot: bool,
    pub at: String,
    pub live: usize,
}

pub fn oldest_live_read() -> Option<OldestLiveRead> {
    let reads = LIVE_READS.lock().unwrap_or_else(PoisonError::into_inner);
    let live = reads.len();
    let oldest = reads.values().min_by_key(|read| read.started)?;
    Some(OldestLiveRead {
        age_ms: oldest.started.elapsed().as_millis(),
        snapshot: oldest.snapshot,
        at: format!("{}:{}", oldest.at.file(), oldest.at.line()),
        live,
    })
}

/// Every live read as (location, is_snapshot), for tests that look for their own entry.
#[cfg(any(test, feature = "test-support"))]
pub fn live_read_locations() -> Vec<(String, bool)> {
    LIVE_READS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .values()
        .map(|read| (format!("{}:{}", read.at.file(), read.at.line()), read.snapshot))
        .collect()
}

pub struct ReadGuard<'a> {
    pub pool: &'a ReadPool,
    pub connection: Option<ReadConnection>,
    /// The connection `Store::read_snapshot` pinned for this thread, shared by every read in it.
    pub pinned: Option<Rc<ReadConnection>>,
    /// Declared last, so it ends after the connection is returned. `None` for a read inside a
    /// pinned snapshot, which `Store::read_snapshot` already registers.
    _live: Option<LiveReadToken>,
}

thread_local! {
    /// While `Store::read_snapshot` runs on this thread: the pool it pinned a connection from,
    /// and that connection, held inside one read transaction.
    /// A request worker's connection, lent without starting a transaction. Reusing it
    /// makes acquisition fallible at the worker boundary rather than inside an infallible get.
    static REQUEST_READER: RefCell<Option<(usize, Rc<ReadConnection>)>> = const { RefCell::new(None) };
    pub static PINNED_READER: RefCell<Option<(usize, Rc<ReadConnection>)>> = const { RefCell::new(None) };
}

/// A write from inside `Store::read_snapshot` commits after the snapshot its thread reads, so
/// the reads that follow it there cannot see it. Nothing writes from a pinned read.
pub fn debug_assert_no_pinned_read() {
    debug_assert!(
        PINNED_READER.with(|slot| slot.borrow().is_none()),
        "a write from inside a pinned read"
    );
}

/// Ends a pinned read on every exit path, panics included.
pub struct PinnedRead<'a> {
    pub pool: &'a ReadPool,
    pub connection: Option<Rc<ReadConnection>>,
    /// Restore an outer snapshot when a different store is read inside it.
    pub previous: Option<(usize, Rc<ReadConnection>)>,
}

impl Drop for PinnedRead<'_> {
    fn drop(&mut self) {
        PINNED_READER.with(|slot| *slot.borrow_mut() = self.previous.take());
        let Some(connection) = self.connection.take() else {
            return;
        };
        connection.progress_handler(0, None::<fn() -> bool>);
        if connection.execute_batch("COMMIT").is_err() && !connection.is_autocommit() {
            let _ = connection.execute_batch("ROLLBACK");
        }
        configure_read_cancellation(&connection);
        // Every guard lent from the pin is gone by now; if one escaped, the pool loses that
        // connection rather than sharing it.
        if let Ok(connection) = Rc::try_unwrap(connection) {
            self.pool.release(connection);
        }
    }
}

impl ReadPool {
    pub fn new(path: &Path, shared_memory: bool) -> Result<Self> {
        let pool = Self {
            idle: Mutex::new(Vec::new()),
            returned: Condvar::new(),
            path: path.to_path_buf(),
            shared_memory,
            counts: Arc::new(ReaderCounts::default()),
        };
        // Open one now, so a store that cannot be read fails to open.
        let connection = pool.open_connection()?;
        pool.release(connection);
        Ok(pool)
    }

    pub fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    fn open_connection(&self) -> Result<ReadConnection> {
        let connection = open_read_connection(&self.path, self.shared_memory)?;
        let open = self.counts.open.fetch_add(1, Ordering::Relaxed) + 1;
        self.counts.peak.fetch_max(open, Ordering::Relaxed);
        self.counts.opened.fetch_add(1, Ordering::Relaxed);
        Ok(ReadConnection {
            connection,
            counts: self.counts.clone(),
        })
    }

    /// Acquire before running an API worker. Connection-open failures are returned
    /// immediately, and cancellation is checked before and after acquisition.
    #[track_caller]
    pub fn try_get(&self) -> Result<ReadGuard<'_>> {
        crate::read_budget::check()?;
        let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let connection = match idle { Some(connection) => connection, None => self.open_connection()? };
        configure_read_cancellation(&connection);
        let guard = ReadGuard { pool: self, connection: Some(connection), pinned: None, _live: Some(register_live_read(false)) };
        crate::read_budget::check()?;
        Ok(guard)
    }

    /// One reader per blocking worker, without BEGIN. Reads still take a snapshot per
    /// statement unless Store::read_snapshot explicitly pins a transaction. Nested scopes
    /// reuse the same loan; unrelated workers acquire their own connection.
    #[track_caller]
    pub fn request_read<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        if REQUEST_READER.with(|slot| slot.borrow().as_ref().is_some_and(|(key, _)| *key == self.key())) {
            return Ok(work());
        }
        let mut guard = self.try_get()?;
        let connection = Rc::new(guard.connection.take().expect("try_get holds a connection"));
        let live = guard._live.take();
        drop(guard);
        struct Loan<'a> {
            pool: &'a ReadPool,
            connection: Option<Rc<ReadConnection>>,
            previous: Option<(usize, Rc<ReadConnection>)>,
            // Keep the registry entry until the entire request loan has returned its reader.
            _live: Option<LiveReadToken>,
        }
        impl Drop for Loan<'_> {
            fn drop(&mut self) {
                REQUEST_READER.with(|slot| *slot.borrow_mut() = self.previous.take());
                let connection = self.connection.take().expect("the loan holds its connection");
                connection.progress_handler(0, None::<fn() -> bool>);
                // A cancelled read must not retain its snapshot in the pool. The connection
                // is query_only; ending a leftover read transaction cannot commit writes.
                if !connection.is_autocommit() {
                    let _ = connection.execute_batch("ROLLBACK");
                }
                if let Ok(connection) = Rc::try_unwrap(connection)
                    && connection.is_autocommit()
                {
                    self.pool.release(connection);
                }
            }
        }
        let previous = REQUEST_READER.with(|slot| slot.replace(Some((self.key(), connection.clone()))));
        let _loan = Loan { pool: self, connection: Some(connection), previous, _live: live };
        Ok(work())
    }

    /// Operational estimates remain available with SQLite MEMSTATUS disabled. These counts
    /// describe connections; page-cache targets are not measurements of process memory.
    pub fn usage(&self) -> ReaderUsage {
        let idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        ReaderUsage {
            open: self.counts.open.load(Ordering::Relaxed),
            idle: idle.len(),
            peak: self.counts.peak.load(Ordering::Relaxed),
            opened: self.counts.opened.load(Ordering::Relaxed),
        }
    }

    #[track_caller]
    pub fn get(&self) -> ReadGuard<'_> {
        crate::read_budget::note_read_site();
        let lookup = |slot: &RefCell<Option<(usize, Rc<ReadConnection>)>>| {
            slot.borrow().as_ref().filter(|(pool, _)| *pool == self.key())
                .map(|(_, connection)| connection.clone())
        };
        let pinned = PINNED_READER.with(lookup).or_else(|| REQUEST_READER.with(lookup));
        if let Some(connection) = &pinned {
            configure_read_cancellation(connection);
            return ReadGuard {
                pool: self,
                connection: None,
                pinned,
                _live: None,
            };
        }
        let waiting = crate::profile::enabled().then(std::time::Instant::now);
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let connection = match idle {
            Some(connection) => connection,
            None => match self.open_connection() {
                Ok(connection) => {
                    crate::profile::note("read connection opened");
                    connection
                }
                Err(error) => {
                    eprintln!("st3: open another read connection: {error:#}; waiting for one");
                    let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
                    loop {
                        if let Some(connection) = idle.pop() {
                            break connection;
                        }
                        idle = self
                            .returned
                            .wait(idle)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                }
            },
        };
        if let Some(waiting) = waiting {
            crate::profile::read_waited(waiting.elapsed());
        }
        configure_read_cancellation(&connection);
        ReadGuard {
            pool: self,
            connection: Some(connection),
            pinned: None,
            _live: Some(register_live_read(false)),
        }
    }

    /// Keep `connection` for the next read, or close it when the pool already holds
    /// `max_idle_read_connections()` of them.
    pub fn release(&self, connection: ReadConnection) {
        // Remove cancellation before reuse by an unrelated request. No other thread can
        // still own this connection; pinned readers return only after their last guard.
        connection.progress_handler(0, None::<fn() -> bool>);
        let _ = connection.busy_timeout(std::time::Duration::from_secs(5));
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < max_idle_read_connections() {
            idle.push(connection);
            self.returned.notify_one();
        }
    }
}

/// The callback consults the worker's current scope, so nested event-query budgets and
/// pinned snapshots use the innermost deadline without caching a guard result.
fn read_busy(attempt: i32) -> bool {
    if attempt >= 500 || crate::read_budget::interrupted() { return false; }
    std::thread::sleep(std::time::Duration::from_millis(10));
    !crate::read_budget::interrupted()
}

fn configure_read_cancellation(connection: &Connection) {
    if crate::read_budget::current().is_some() {
        connection.progress_handler(1000, Some(crate::read_budget::interrupted));
        connection.busy_handler(Some(read_busy)).expect("install a read busy handler");
    }
}

impl Deref for ReadGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        let connection = self.pinned
            .as_deref()
            .or(self.connection.as_ref())
            .expect("a read guard always has a connection");
        // Cheap queries can finish before the normal progress interval. Once cancelled,
        // the next operation on this guard must fail too, including a cached statement.
        if crate::read_budget::interrupted() {
            connection.progress_handler(1, Some(|| true));
        }
        connection
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // A pinned connection goes back when its snapshot ends, not here.
        if let Some(connection) = self.connection.take() {
            self.pool.release(connection);
        }
    }
}

/// Nanoseconds every SQLite statement in this process has taken, from SQLite's profile hook.
pub static SQLITE_NANOS: AtomicU64 = AtomicU64::new(0);

/// The part of that time, and the count, of `COMMIT` statements, which wait for a disk flush.
pub static SQLITE_COMMITS: AtomicU64 = AtomicU64::new(0);

pub static SQLITE_COMMIT_NANOS: AtomicU64 = AtomicU64::new(0);

pub fn record_sqlite_time(statement: &str, duration: std::time::Duration) {
    #[cfg(any(test, feature = "test-support"))]
    STATEMENTS_RUN.with(|run| run.set(run.get() + 1));
    crate::profile::sql(statement, duration);
    crate::performance::record_query(statement, duration);
    #[cfg(any(test, feature = "test-support"))]
    histogram::record(statement, duration);
    SQLITE_NANOS.fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    if statement == "COMMIT" {
        SQLITE_COMMITS.fetch_add(1, Ordering::Relaxed);
        SQLITE_COMMIT_NANOS.fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Record every statement `connection` runs: its time in the profile, and with `test-support`,
/// its work in [`work`].
pub fn observe(connection: &mut Connection) {
    #[cfg(any(test, feature = "test-support"))]
    work::count(connection);
    connection.profile(Some(record_sqlite_time));
}

/// Every statement this process ran since the last [`histogram::take`], by normalized text, so a
/// test can say which statement shapes a request ran and how many times each.
#[cfg(any(test, feature = "test-support"))]
pub mod histogram {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    /// Count, total nanoseconds and the slowest single run of one statement shape.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Shape {
        pub count: u64,
        pub total_ns: u64,
        pub max_ns: u64,
    }

    static SHAPES: Mutex<BTreeMap<String, Shape>> = Mutex::new(BTreeMap::new());

    pub fn record(statement: &str, duration: Duration) {
        let shape = crate::performance::normalize_query(statement);
        let mut shapes = SHAPES.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = shapes.entry(shape).or_default();
        let ns = duration.as_nanos() as u64;
        entry.count += 1;
        entry.total_ns += ns;
        entry.max_ns = entry.max_ns.max(ns);
    }

    /// The shapes recorded so far, emptying the table.
    pub fn take() -> BTreeMap<String, Shape> {
        std::mem::take(&mut *SHAPES.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The work SQLite did for every statement this process ran, read from each statement's own
/// counters as it finishes, so a test can see whether a request's work grows with the store.
/// Timings vary from machine to machine; these counts do not.
#[cfg(any(test, feature = "test-support"))]
pub mod work {
    use std::collections::BTreeMap;
    use std::ffi::{c_int, c_uint, c_void};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, PoisonError};

    use rusqlite::{Connection, ffi};

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SqliteWork {
        pub statements: u64,
        /// Virtual machine instructions, which every row read, compared or written costs.
        pub vm_steps: u64,
        /// Steps forward through a table without an index.
        pub fullscan_steps: u64,
        /// Sorts SQLite ran because no index gave the order.
        pub sorts: u64,
        /// Rows put into indexes SQLite built for one statement because none existed.
        pub autoindex_rows: u64,
    }

    impl std::ops::Sub for SqliteWork {
        type Output = SqliteWork;

        fn sub(self, before: SqliteWork) -> SqliteWork {
            SqliteWork {
                statements: self.statements - before.statements,
                vm_steps: self.vm_steps - before.vm_steps,
                fullscan_steps: self.fullscan_steps - before.fullscan_steps,
                sorts: self.sorts - before.sorts,
                autoindex_rows: self.autoindex_rows - before.autoindex_rows,
            }
        }
    }

    static STATEMENTS: AtomicU64 = AtomicU64::new(0);
    static VM_STEPS: AtomicU64 = AtomicU64::new(0);
    static FULLSCAN_STEPS: AtomicU64 = AtomicU64::new(0);
    static SORTS: AtomicU64 = AtomicU64::new(0);
    static AUTOINDEX_ROWS: AtomicU64 = AtomicU64::new(0);

    /// Everything counted so far, in every connection of this process.
    pub fn total() -> SqliteWork {
        SqliteWork {
            statements: STATEMENTS.load(Ordering::Relaxed),
            vm_steps: VM_STEPS.load(Ordering::Relaxed),
            fullscan_steps: FULLSCAN_STEPS.load(Ordering::Relaxed),
            sorts: SORTS.load(Ordering::Relaxed),
            autoindex_rows: AUTOINDEX_ROWS.load(Ordering::Relaxed),
        }
    }

    /// Count `connection`'s statements. SQLite reports each one when it starts and when it
    /// finishes, also when a trigger or a foreign-key check did the work inside it.
    pub(super) fn count(connection: &Connection) {
        // SAFETY: the callback only reads the statement's counters, and the handle stays valid
        // for the connection's life, which ends the registration with it.
        unsafe {
            ffi::sqlite3_trace_v2(
                connection.handle(),
                (ffi::SQLITE_TRACE_STMT | ffi::SQLITE_TRACE_PROFILE) as c_uint,
                Some(traced),
                std::ptr::null_mut(),
            );
        }
    }

    const COUNTERS: [c_int; 4] = [
        ffi::SQLITE_STMTSTATUS_VM_STEP,
        ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP,
        ffi::SQLITE_STMTSTATUS_SORT,
        ffi::SQLITE_STMTSTATUS_AUTOINDEX,
    ];

    /// Each running statement's counters when it started. A statement's counters add up over
    /// its runs until someone resets them, and tests read them too, so this never resets them.
    static STARTED: Mutex<BTreeMap<usize, [u64; 4]>> = Mutex::new(BTreeMap::new());

    unsafe extern "C" fn traced(
        event: c_uint,
        _context: *mut c_void,
        statement: *mut c_void,
        _detail: *mut c_void,
    ) -> c_int {
        let key = statement as usize;
        let statement = statement.cast::<ffi::sqlite3_stmt>();
        // SAFETY: SQLite passes the statement that started or finished.
        let read = |counter: c_int| unsafe { ffi::sqlite3_stmt_status(statement, counter, 0) };
        let now = COUNTERS.map(|counter| u64::try_from(read(counter)).unwrap_or(0));
        let mut started = STARTED.lock().unwrap_or_else(PoisonError::into_inner);
        if event == ffi::SQLITE_TRACE_STMT as c_uint {
            // A trigger's start reports the statement again; the first start counts.
            started.entry(key).or_insert(now);
            return 0;
        }
        let before = started.remove(&key).unwrap_or([0; 4]);
        let spent = |index: usize| now[index].saturating_sub(before[index]);
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
        VM_STEPS.fetch_add(spent(0), Ordering::Relaxed);
        FULLSCAN_STEPS.fetch_add(spent(1), Ordering::Relaxed);
        SORTS.fetch_add(spent(2), Ordering::Relaxed);
        AUTOINDEX_ROWS.fetch_add(spent(3), Ordering::Relaxed);
        0
    }
}

pub fn open_read_connection(path: &Path, shared_memory: bool) -> Result<Connection> {
    let flags = if shared_memory {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let mut connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("open st read connection {}", path.display()))?;
    crate::read_budget::check()?;
    configure_read_cancellation(&connection);
    observe(&mut connection);
    connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    connection.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA query_only = ON;",
    )?;
    connection.pragma_update(None, "cache_size", -(read_cache_kib() as i64))?;
    configure_read_cancellation(&connection);
    Ok(connection)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// SQLite statements this thread ran, so a test can see how a read's work grows.
    pub static STATEMENTS_RUN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_payload_values(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT value FROM payload ORDER BY sequence")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn idle_checkpoint_recycles_the_wal_after_a_reader_releases_its_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES (zeroblob(4096));",
            )
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT value FROM payload;")
            .unwrap();
        writer
            .execute_batch("UPDATE payload SET value=zeroblob(8192);")
            .unwrap();
        let checkpoint = Connection::open(&path).unwrap();
        let wal = path.with_extension("sqlite3-wal");
        assert!(!checkpoint_idle_wal(&checkpoint).unwrap());
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        // A failed recycle cannot block or lose a write while the old snapshot lives.
        writer
            .execute_batch("INSERT INTO payload VALUES (zeroblob(4096));")
            .unwrap();
        reader.execute_batch("COMMIT").unwrap();
        assert!(checkpoint_idle_wal(&checkpoint).unwrap());
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(
            reader
                .query_row("SELECT sum(length(value)) FROM payload", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            12288
        );
    }

    #[test]
    fn idle_checkpoint_defers_fully_backfilled_wal_until_current_reader_releases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE payload(sequence INTEGER PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO payload VALUES (0, 'original');",
            )
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        assert_eq!(wal_payload_values(&reader), ["original"]);
        let checkpoint = Connection::open(&path).unwrap();
        let wal = path.with_extension("sqlite3-wal");

        // This reader pins the current WAL end, not an older snapshot that limits PASSIVE.
        let (busy, frames, backfilled): (i32, i32, i32) = checkpoint
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 0);
        assert!(frames > 0);
        assert_eq!(frames, backfilled);
        assert!(!checkpoint_idle_wal(&checkpoint).unwrap());
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        let (busy, frames, backfilled): (i32, i32, i32) = checkpoint
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(busy, 1, "the readmark prevents recycling, not backfilling");
        assert!(frames > 0);
        assert_eq!(frames, backfilled);

        // A busy TRUNCATE neither blocks the next commit nor changes the pinned snapshot.
        writer
            .execute("INSERT INTO payload VALUES (1, 'committed while pinned')", [])
            .unwrap();
        assert_eq!(wal_payload_values(&reader), ["original"]);
        assert_eq!(
            wal_payload_values(&writer),
            ["original", "committed while pinned"]
        );
        reader.execute_batch("COMMIT").unwrap();
        assert!(checkpoint_idle_wal(&checkpoint).unwrap());
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(
            wal_payload_values(&reader),
            ["original", "committed while pinned"]
        );
    }

    #[test]
    fn idle_checkpoint_defers_recycling_under_sustained_overlapping_readers_and_writes() {
        use std::sync::mpsc::sync_channel;
        use std::time::Duration;

        const WAIT: Duration = Duration::from_secs(5);
        const VALUES: [&str; 9] = [
            "seed", "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE payload(sequence INTEGER PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO payload VALUES (0, 'seed');",
            )
            .unwrap();
        let checkpoint = Connection::open(&path).unwrap();
        let wal = path.with_extension("sqlite3-wal");

        std::thread::scope(|scope| {
            let start_reader = |last: usize| {
                let path = &path;
                let (ready, pinned) = sync_channel(1);
                let (release, released) = sync_channel(1);
                let (done, finished) = sync_channel(1);
                let thread = scope.spawn(move || {
                    let reader = Connection::open(path).unwrap();
                    reader.busy_timeout(Duration::ZERO).unwrap();
                    reader.execute_batch("BEGIN").unwrap();
                    assert_eq!(wal_payload_values(&reader), VALUES[..=last]);
                    ready.send(()).unwrap();
                    released.recv_timeout(WAIT).unwrap();
                    assert_eq!(wal_payload_values(&reader), VALUES[..=last]);
                    reader.execute_batch("COMMIT").unwrap();
                    done.send(()).unwrap();
                });
                pinned.recv_timeout(WAIT).unwrap();
                (release, finished, thread)
            };

            let mut active = start_reader(0);
            for (sequence, value) in VALUES.iter().enumerate().skip(1) {
                // Every commit completes while the previous snapshot is still pinned.
                writer
                    .execute(
                        "INSERT INTO payload VALUES (?1, ?2)",
                        rusqlite::params![sequence as i64, value],
                    )
                    .unwrap();
                assert_eq!(wal_payload_values(&writer), VALUES[..=sequence]);
                let next = start_reader(sequence);
                assert!(!checkpoint_idle_wal(&checkpoint).unwrap());
                assert!(std::fs::metadata(&wal).unwrap().len() > 0);

                // Acquisition is acknowledged before releasing the previous reader:
                // repeated handoffs deliberately provide no reader-free idle gap.
                active.0.send(()).unwrap();
                active.1.recv_timeout(WAIT).unwrap();
                active.2.join().unwrap();
                active = next;

                let (busy, frames, backfilled): (i32, i32, i32) = checkpoint
                    .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .unwrap();
                assert_eq!(busy, 0);
                assert!(frames > 0);
                assert_eq!(frames, backfilled);
                assert!(!checkpoint_idle_wal(&checkpoint).unwrap());
                assert!(std::fs::metadata(&wal).unwrap().len() > 0);
            }

            active.0.send(()).unwrap();
            active.1.recv_timeout(WAIT).unwrap();
            active.2.join().unwrap();
            assert!(checkpoint_idle_wal(&checkpoint).unwrap());
            assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
            assert_eq!(wal_payload_values(&writer), VALUES);
        });
    }


    #[test]
    fn repeated_bursts_of_reads_reuse_connections_instead_of_opening_new_ones() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        // A read-only open needs the file to exist.
        rusqlite::Connection::open(&path).unwrap();
        let pool = ReadPool::new(&path, false).unwrap();
        let opened = || pool.usage().opened;
        let burst = |pool: &ReadPool| {
            let guards: Vec<_> = (0..40).map(|_| pool.get()).collect();
            drop(guards);
        };
        burst(&pool);
        // Forty simultaneous reads exceed the old 32-reader retention ceiling.
        // The first wave opens 39 connections besides the seed; the second must
        // reuse them rather than parse the schema under SQLite's allocator mutex.
        assert_eq!(opened(), 40);
        burst(&pool);
        assert_eq!(opened(), 40, "the second wave must not open connections");
        let usage = pool.usage();
        assert_eq!((usage.open, usage.idle, usage.peak), (40, 40, 40));
        for connection in pool.idle.lock().unwrap().iter() {
            let cache: i64 = connection
                .query_row("PRAGMA cache_size", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cache, -(read_cache_kib() as i64));
        }
    }

    #[test]
    fn reader_cache_override_rejects_values_that_disable_the_cache_limit() {
        for invalid in [
            None,
            Some(""),
            Some("bad"),
            Some("0"),
            Some("-1024"),
            Some("2147483648"),
        ] {
            assert_eq!(configured_read_cache_kib(invalid), READ_CACHE_KIB);
        }
        assert_eq!(configured_read_cache_kib(Some("1024")), 1024);
        assert_eq!(configured_read_cache_kib(Some("8192")), 8192);
    }

    #[test]
    fn counts_follow_a_reader_shared_by_a_snapshot_until_its_last_owner_drops() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let pool = ReadPool::new(&path, false).unwrap();
        let mut guard = pool.get();
        let connection = Rc::new(guard.connection.take().unwrap());
        drop(guard);
        let escaped = connection.clone();
        drop(PinnedRead {
            pool: &pool,
            connection: Some(connection),
            previous: None,
        });
        assert_eq!((pool.usage().open, pool.usage().idle), (1, 0));
        drop(escaped);
        assert_eq!((pool.usage().open, pool.usage().idle), (0, 0));
        drop(pool.get());
        assert_eq!((pool.usage().open, pool.usage().opened), (1, 2));
    }

    #[test]
    fn burst_and_retained_counts_include_connections_closed_above_retention() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        rusqlite::Connection::open(&path).unwrap();
        let pool = ReadPool::new(&path, false).unwrap();
        let burst = MAX_IDLE_READ_CONNECTIONS + 8;
        let guards: Vec<_> = (0..burst).map(|_| pool.get()).collect();
        assert_eq!(
            (pool.usage().open, pool.usage().idle, pool.usage().peak),
            (burst, 0, burst)
        );
        drop(guards);
        let retained = burst.min(max_idle_read_connections());
        assert_eq!(
            (pool.usage().open, pool.usage().idle, pool.usage().peak),
            (retained, retained, burst)
        );
    }
}

#[cfg(test)]
mod commit_observer_tests {
    use super::*;
    use std::sync::mpsc::{self, TryRecvError};

    fn writer() -> WriterConnection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE allowed(authority INTEGER PRIMARY KEY);
                 INSERT INTO allowed VALUES (1), (2);
                 CREATE TABLE claims(
                     store_index INTEGER PRIMARY KEY AUTOINCREMENT,
                     authority INTEGER NOT NULL REFERENCES allowed(authority)
                         DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();
        WriterConnection::new(connection, Arc::new(AtomicU64::new(0)))
    }

    fn queued_claim(writer: &WriterConnection) -> mpsc::Receiver<Result<(), String>> {
        let (done, answer) = mpsc::sync_channel(1);
        writer.send(WriterJob::Batched {
            run: Box::new(|transaction| {
                transaction
                    .execute("INSERT INTO claims(authority) VALUES (1)", [])
                    .unwrap();
                true
            }),
            profile: None,
            wait: None,
            done,
        });
        answer
    }

    #[test]
    fn batched_commit_observer_reads_committed_authority_before_acknowledgement() {
        let writer = writer();
        let index = writer.committed_index.clone();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let _observer = writer.observe_commits(move |connection| {
            let authority: i64 = connection
                .query_row("SELECT authority FROM claims", [], |row| row.get(0))
                .unwrap();
            entered
                .send((
                    connection.is_autocommit(),
                    index.load(Ordering::Acquire),
                    authority,
                ))
                .unwrap();
            // Disconnecting this gate also releases the callback if the test unwinds.
            let _ = released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
        });
        // Drop the gate before the observer if this test unwinds while a callback is waiting.
        let unblock = release;
        let answer = queued_claim(&writer);
        let committed = observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let premature = answer.try_recv();
        unblock.send(()).unwrap();
        assert_eq!(committed, (true, 1, 1));
        assert!(matches!(premature, Err(TryRecvError::Empty)));
        answer.recv().unwrap().unwrap();
    }

    #[test]
    fn lent_writes_notify_before_guard_drop_returns_and_stop_after_observer_drop() {
        let writer = writer();
        let (recorded, observations) = mpsc::channel();
        let index = writer.committed_index.clone();
        let observer = writer.observe_commits(move |connection| {
            let authority: i64 = connection
                .query_row("SELECT authority FROM claims", [], |row| row.get(0))
                .unwrap();
            recorded
                .send((
                    connection.is_autocommit(),
                    index.load(Ordering::Acquire),
                    authority,
                ))
                .unwrap();
        });
        {
            let mut connection = writer.write();
            let transaction = connection.transaction().unwrap();
            transaction
                .execute("INSERT INTO claims(authority) VALUES (1)", [])
                .unwrap();
            transaction.commit().unwrap();
            assert!(matches!(observations.try_recv(), Err(TryRecvError::Empty)));
        }
        assert_eq!(observations.recv().unwrap(), (true, 1, 1));
        drop(observer);
        writer
            .batched(|transaction| transaction.execute("UPDATE claims SET authority=2", []))
            .unwrap()
            .unwrap();
        assert!(observations.try_recv().is_err());
    }

    #[test]
    fn a_failed_commit_does_not_notify_observers() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        let _observer = writer.observe_commits(move |_| {
            called.fetch_add(1, Ordering::Relaxed);
        });
        // The insert succeeds, but its deferred foreign key rejects the outer commit.
        assert!(
            writer
                .batched(|transaction| {
                    transaction.execute("INSERT INTO claims(authority) VALUES (99)", [])
                })
                .is_err()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        writer
            .batched(|transaction| {
                transaction.execute("INSERT INTO claims(authority) VALUES (1)", [])
            })
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(writer.committed_index.load(Ordering::Acquire), 1);
    }

    #[test]
    fn dropping_an_observer_deactivates_an_in_flight_commit_snapshot() {
        let writer = writer();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let _first = writer.observe_commits(move |_| {
            entered.send(()).unwrap();
            let _ = released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
        });
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        let second = writer.observe_commits(move |_| {
            called.fetch_add(1, Ordering::Relaxed);
        });
        let unblock = release;
        let answer = queued_claim(&writer);
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(second);
        unblock.send(()).unwrap();
        answer.recv().unwrap().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn observer_handles_can_outlive_writer_shutdown_without_retaining_callbacks() {
        let writer = writer();
        let calls = Arc::new(AtomicU64::new(0));
        let retained = Arc::downgrade(&calls);
        let observer = writer.observe_commits(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert!(retained.upgrade().is_some());
        drop(writer);
        assert!(retained.upgrade().is_none());
        drop(observer);
    }

    #[test]
    fn the_last_writer_owner_can_be_released_from_a_commit_observer() {
        let writer = Arc::new(writer());
        let owner = Arc::downgrade(&writer);
        let retained = owner.clone();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let observer = writer.observe_commits(move |_| {
            let owner = owner.upgrade().unwrap();
            entered.send(()).unwrap();
            let _ = released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
            drop(owner);
        });
        let unblock = release;
        let answer = queued_claim(&writer);
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(writer);
        unblock.send(()).unwrap();
        answer
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        drop(observer);
    }

    #[test]
    fn a_callback_can_release_the_last_owner_of_its_observer_handle() {
        let writer = writer();
        let lease = Arc::new(Mutex::new(None::<CommitObserver>));
        let owner = Arc::downgrade(&lease);
        let retained = owner.clone();
        let (entered, observed) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let released = Mutex::new(released);
        let observer = writer.observe_commits(move |_| {
            let lease = owner.upgrade().unwrap();
            entered.send(()).unwrap();
            let _ = released
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv();
            drop(lease);
        });
        *lease.lock().unwrap_or_else(PoisonError::into_inner) = Some(observer);
        let unblock = release;
        let answer = queued_claim(&writer);
        observed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(lease);
        unblock.send(()).unwrap();
        answer
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        writer
            .batched(|transaction| transaction.execute("UPDATE claims SET authority=2", []))
            .unwrap()
            .unwrap();
    }

    #[test]
    fn an_observer_panic_never_acknowledges_a_committed_batch() {
        let writer = writer();
        let _observer = writer.observe_commits(|_| panic!("commit observer failed"));
        let result = writer.batched(|transaction| {
            transaction.execute("INSERT INTO claims(authority) VALUES (1)", [])
        });
        assert!(result.is_err());
        // The commit happened before the observer panicked; it must not be reported as success.
        assert_eq!(writer.committed_index.load(Ordering::Acquire), 1);
    }
}

#[cfg(test)]
mod read_cancellation_tests {
    use super::*;
    use crate::read_budget::{self, ReadBudget};
    use std::time::Duration;

    fn pool() -> (tempfile::TempDir, Arc<ReadPool>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("read.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE data(value); INSERT INTO data VALUES(1)").unwrap();
        drop(writer);
        let pool = Arc::new(ReadPool::new(&path, false).unwrap());
        (directory, pool)
    }

    #[test]
    fn cancellation_stops_live_sql_and_the_reader_is_clean_for_another_request() {
        let (_directory, pool) = pool();
        let budget = ReadBudget::new("/sql", Duration::from_secs(30));
        let worker_budget = budget.clone();
        let worker_pool = pool.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            read_budget::with(Some(worker_budget), || worker_pool.request_read(|| {
                let reader = worker_pool.get();
                started_tx.send(()).unwrap();
                reader.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))
            }))
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        budget.cancel();
        let error = worker.join().unwrap().unwrap().unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::OperationInterrupted));
        assert_eq!(pool.usage().idle, 1);
        let reader = pool.get();
        assert!(reader.is_autocommit());
        assert_eq!(reader.query_row("SELECT value FROM data", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn an_expired_request_is_rejected_before_a_connection_is_opened_or_lent() {
        let (_directory, pool) = pool();
        let before = pool.usage();
        let error = read_budget::with(Some(ReadBudget::new("/expired", Duration::ZERO)), || pool.try_get().map(|_| ())).unwrap_err();
        assert_eq!(error.downcast_ref::<crate::error::Error>().unwrap().code, "read-deadline");
        let after = pool.usage();
        assert_eq!(after.idle, before.idle);
        assert_eq!(after.opened, before.opened);
    }

    #[test]
    fn failed_acquisition_returns_an_error_instead_of_waiting_for_another_reader() {
        let (directory, pool) = pool();
        // Keep its one reader in use and make another open impossible, using only a fixture.
        let held = pool.get();
        std::fs::remove_file(directory.path().join("read.sqlite")).unwrap();
        let error = read_budget::with(Some(ReadBudget::new("/open-failed", Duration::from_secs(1))), || pool.try_get().map(|_| ())).unwrap_err();
        assert!(error.to_string().contains("open st read connection"));
        drop(held);
    }

    #[test]
    fn request_loan_stays_in_the_live_reader_registry_until_the_reader_returns() {
        let (_directory, pool) = pool();
        let at = format!("{}:{}", file!(), line!() + 1);
        pool.request_read(|| {
            assert_eq!(live_read_locations().iter().filter(|(where_, snapshot)| where_ == &at && !snapshot).count(), 1);
            { let _reader = pool.get(); }
            pool.request_read(|| assert_eq!(live_read_locations().iter().filter(|(where_, snapshot)| where_ == &at && !snapshot).count(), 1)).unwrap();
            assert_eq!(live_read_locations().iter().filter(|(where_, snapshot)| where_ == &at && !snapshot).count(), 1);
        }).unwrap();
        assert!(!live_read_locations().iter().any(|(where_, _)| where_ == &at));
        assert_eq!(pool.usage().idle, 1);
    }

    #[test]
    fn worker_loans_are_autocommit_and_nested_scopes_do_not_end_each_other() {
        let (_directory, pool) = pool();
        let budget = ReadBudget::new("/loan", Duration::from_secs(30));
        read_budget::with(Some(budget), || pool.request_read(|| {
            assert!(pool.get().is_autocommit());
            pool.request_read(|| assert!(pool.get().is_autocommit())).unwrap();
            assert!(pool.get().is_autocommit());
        })).unwrap();
        assert_eq!(pool.usage().idle, 1);
    }
}
