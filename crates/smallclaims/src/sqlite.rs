//! The store's SQLite connections: one writer thread that batches writes, and a pool of read
//! connections that a thread can pin to one snapshot.

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, Transaction};

use crate::store::current_index;

/// Read connections a store keeps open between reads; more open while more reads run at once.
/// Each caches up to 8 MiB of pages.
pub const IDLE_READ_CONNECTIONS: usize = 32;

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

struct CommitObserverState {
    run: Option<Arc<dyn Fn(&Connection) + Send + Sync>>,
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
    /// When profiling, when this thread took the writer.
    pub acquired: Option<std::time::Instant>,
    /// The connection's changed-row count when it was lent, so the rows this thread changed are
    /// noted for it when it gives the connection back; see `touched::writes`.
    pub changes_at_lend: u64,
}

impl WriterConnection {
    pub fn new(connection: Connection, committed_index: Arc<AtomicU64>) -> Self {
        let (jobs, queue) = std::sync::mpsc::channel::<WriterJob>();
        let index = committed_index.clone();
        let batches = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let counted = batches.clone();
        let observers = Arc::new(CommitObservers::default());
        let observed = observers.clone();
        let thread = std::thread::Builder::new()
            .name("st3-writer".into())
            .spawn(move || write_queue(connection, queue, &index, &counted, &observed))
            .expect("the writer thread starts");
        Self {
            jobs: Mutex::new(Some(jobs)),
            thread: Mutex::new(Some(thread)),
            committed_index,
            observers,
            batches,
        }
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
    observers: &CommitObservers,
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
                    observers,
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
    observers: &CommitObservers,
) -> Option<WriterJob> {
    let started = std::time::Instant::now();
    let mut answers = Vec::new();
    let mut lend = None;
    let (transaction, mut failure) =
        match connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
            Ok(transaction) => (Some(transaction), None),
            Err(error) => (None, Some(error)),
        };
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
                failure = savepoint.err();
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
        (Some(transaction), None) => transaction.commit(),
        // Dropping the transaction rolls back every write in the batch.
        (_, Some(error)) => Err(error),
        (None, None) => unreachable!("a batch without a transaction failed to begin"),
    };
    if let Ok(index) = current_index(connection) {
        committed_index.store(index, Ordering::Release);
    }
    if committed.is_ok() {
        observers.notify(connection);
    }
    let committed = committed.map_err(|error| error.to_string());
    for done in answers {
        let _ = done.send(committed.clone());
    }
    lend
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
/// ever ran at once, keeps up to `IDLE_READ_CONNECTIONS` of them between reads, and closes them
/// with the store. Reads see the last committed state and, in WAL mode, never wait for the writer.
pub struct ReadPool {
    pub idle: Mutex<Vec<Connection>>,
    /// Wakes a read waiting for an idle connection, which happens only when the operating system
    /// refuses another one, for example past the open file limit.
    pub returned: Condvar,
    pub path: PathBuf,
    pub shared_memory: bool,
}

pub struct ReadGuard<'a> {
    pub pool: &'a ReadPool,
    pub connection: Option<Connection>,
    /// The connection `Store::read_snapshot` pinned for this thread, shared by every read in it.
    pub pinned: Option<Rc<Connection>>,
}

thread_local! {
    /// While `Store::read_snapshot` runs on this thread: the pool it pinned a connection from,
    /// and that connection, held inside one read transaction.
    pub static PINNED_READER: RefCell<Option<(usize, Rc<Connection>)>> = const { RefCell::new(None) };
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
    pub connection: Option<Rc<Connection>>,
}

impl Drop for PinnedRead<'_> {
    fn drop(&mut self) {
        PINNED_READER.with(|slot| slot.borrow_mut().take());
        let Some(connection) = self.connection.take() else {
            return;
        };
        let _ = connection.execute_batch("COMMIT");
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
        };
        // Open one now, so a store that cannot be read fails to open.
        let connection = open_read_connection(&pool.path, pool.shared_memory)?;
        pool.release(connection);
        Ok(pool)
    }

    pub fn key(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    pub fn get(&self) -> ReadGuard<'_> {
        let pinned = PINNED_READER.with(|slot| {
            slot.borrow()
                .as_ref()
                .filter(|(pool, _)| *pool == self.key())
                .map(|(_, connection)| connection.clone())
        });
        if pinned.is_some() {
            return ReadGuard {
                pool: self,
                connection: None,
                pinned,
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
            None => match open_read_connection(&self.path, self.shared_memory) {
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
        ReadGuard {
            pool: self,
            connection: Some(connection),
            pinned: None,
        }
    }

    /// Keep `connection` for the next read, or close it when enough are idle already.
    pub fn release(&self, connection: Connection) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < IDLE_READ_CONNECTIONS {
            idle.push(connection);
            self.returned.notify_one();
        }
    }
}

impl Deref for ReadGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.pinned
            .as_deref()
            .or(self.connection.as_ref())
            .expect("a read guard always has a connection")
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
    observe(&mut connection);
    connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    connection.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -8192;
         PRAGMA query_only = ON;",
    )?;
    Ok(connection)
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// SQLite statements this thread ran, so a test can see how a read's work grows.
    pub static STATEMENTS_RUN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
