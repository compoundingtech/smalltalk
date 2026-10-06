//! The store's SQLite connections: one writer thread that batches writes, and a pool of read
//! connections that a thread can pin to one snapshot.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, Transaction};

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

thread_local! {
    static CONTROL_WRITES: Cell<bool> = const { Cell::new(false) };
}

/// Give writes enqueued by `f` on this thread control-plane admission priority.
///
/// The scope nests and restores its previous priority even when `f` panics; spawned threads do
/// not inherit it. Priority never interrupts an active transaction or a lent connection. Control
/// writes run before queued ordinary work, except that one ordinary turn is reserved after eight
/// control turns. Each control batched write commits without batching ordinary work behind it.
pub fn with_control_writes<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTROL_WRITES.with(|priority| priority.set(self.0));
        }
    }
    let _restore = Restore(CONTROL_WRITES.with(|priority| priority.replace(true)));
    f()
}

/// The store's only write connection, owned by one writer thread. Ordinary writes queue in
/// arrival order; [`with_control_writes`] reserves fair priority admission at transaction
/// boundaries. Batched writes run in savepoints of one transaction and hear back after commit.
/// `write` lends the connection until the guard drops, for callers managing their own
/// transactions. Nothing else ever takes SQLite's write lock.
pub struct WriterConnection {
    pub jobs: Mutex<Option<WriterSender>>,
    pub thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    pub committed_index: Arc<AtomicU64>,
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

const CONTROL_TURN_LIMIT: usize = 8;

#[derive(Default)]
struct WriterQueueState {
    ordinary: VecDeque<WriterJob>,
    control: VecDeque<WriterJob>,
    control_turns: usize,
    senders: usize,
    stopped: bool,
}

struct WriterQueue {
    state: Mutex<WriterQueueState>,
    ready: Condvar,
}

/// An enqueue handle to the writer. Clones keep the queue open, like a channel sender.
pub struct WriterSender {
    queue: Arc<WriterQueue>,
}

impl WriterSender {
    pub fn send(&self, job: WriterJob) -> Result<(), std::sync::mpsc::SendError<WriterJob>> {
        let control = CONTROL_WRITES.with(Cell::get);
        let mut state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopped {
            return Err(std::sync::mpsc::SendError(job));
        }
        if control {
            state.control.push_back(job);
        } else {
            state.ordinary.push_back(job);
        }
        drop(state);
        self.queue.ready.notify_one();
        Ok(())
    }

    /// Queued control and ordinary jobs, excluding the active transaction or connection loan.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_counts(&self) -> (usize, usize) {
        let state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.control.len(), state.ordinary.len())
    }
}

impl Clone for WriterSender {
    fn clone(&self) -> Self {
        self.queue
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .senders += 1;
        Self {
            queue: self.queue.clone(),
        }
    }
}

impl Drop for WriterSender {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.senders -= 1;
        drop(state);
        self.queue.ready.notify_one();
    }
}

struct WriterReceiver {
    queue: Arc<WriterQueue>,
}

impl WriterReceiver {
    fn recv(&self) -> Option<(WriterJob, bool)> {
        let mut state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !state.control.is_empty()
                && (state.control_turns < CONTROL_TURN_LIMIT || state.ordinary.is_empty())
            {
                state.control_turns = (state.control_turns + 1).min(CONTROL_TURN_LIMIT);
                return state.control.pop_front().map(|job| (job, true));
            }
            if let Some(job) = state.ordinary.pop_front() {
                state.control_turns = 0;
                return Some((job, false));
            }
            if state.senders == 0 {
                return None;
            }
            state.control_turns = 0;
            state = self.queue.ready.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Do not consume a loan or a control job before committing the current ordinary batch:
    /// either must compete for admission again at that transaction boundary.
    fn next_in_batch(&self) -> Option<WriterJob> {
        let mut state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.control.is_empty()
            && matches!(state.ordinary.front(), Some(WriterJob::Batched { .. }))
        {
            state.ordinary.pop_front()
        } else {
            None
        }
    }
}

impl Drop for WriterReceiver {
    fn drop(&mut self) {
        let mut state = self.queue.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopped = true;
        // On writer failure, drop every borrowed closure before its completion sender. No
        // caller can return with a closure still queued, even while sender clones survive.
        let ordinary = std::mem::take(&mut state.ordinary);
        let control = std::mem::take(&mut state.control);
        drop(state);
        drop(ordinary);
        drop(control);
    }
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
    /// When profiling, when this thread took the writer.
    pub acquired: Option<std::time::Instant>,
    /// The connection's changed-row count when it was lent, so the rows this thread changed are
    /// noted for it when it gives the connection back; see `touched::writes`.
    pub changes_at_lend: u64,
}

impl WriterConnection {
    pub fn new(connection: Connection, committed_index: Arc<AtomicU64>) -> Self {
        let queue = Arc::new(WriterQueue {
            state: Mutex::new(WriterQueueState {
                senders: 1,
                ..WriterQueueState::default()
            }),
            ready: Condvar::new(),
        });
        let jobs = WriterSender {
            queue: queue.clone(),
        };
        let queue = WriterReceiver { queue };
        let index = committed_index.clone();
        let batches = Arc::new((AtomicU64::new(0), AtomicU64::new(0)));
        let counted = batches.clone();
        let thread = std::thread::Builder::new()
            .name("st3-writer".into())
            .spawn(move || write_queue(connection, queue, &index, &counted))
            .expect("the writer thread starts");
        Self {
            jobs: Mutex::new(Some(jobs)),
            thread: Mutex::new(Some(thread)),
            committed_index,
            batches,
        }
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

    /// The writer connection itself, lent until the guard drops, in FIFO order within this
    /// thread's admission class. A panic while it is lent rolls back the open transaction as it
    /// unwinds and still gives the connection back, so it cannot disable the store.
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
            let _ = thread.join();
        }
    }
}

/// The writer thread: admit control and ordinary turns fairly at transaction boundaries.
fn write_queue(
    mut connection: Connection,
    queue: WriterReceiver,
    committed_index: &AtomicU64,
    batches: &(AtomicU64, AtomicU64),
) {
    while let Some((job, control)) = queue.recv() {
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
                run_write_batch(&mut connection, batched, control, &queue, committed_index, batches);
            }
        }
    }
}

/// Run an ordinary batch in savepoints, or one control write alone, then commit and answer.
/// A queued control write or loan ends an ordinary batch without being dequeued; admission is
/// decided after commit, not before. Size, time and failure boundaries still apply.
fn run_write_batch(
    connection: &mut Connection,
    first: WriterJob,
    control: bool,
    queue: &WriterReceiver,
    committed_index: &AtomicU64,
    batches: &(AtomicU64, AtomicU64),
) {
    let started = std::time::Instant::now();
    let mut answers = Vec::new();
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
            unreachable!("only batched jobs enter a write batch");
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
        if !control
            && failure.is_none()
            && answers.len() < WRITE_BATCH_LIMIT
            && started.elapsed() < WRITE_BATCH_WINDOW
        {
            job = queue.next_in_batch();
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
    let committed = committed.map_err(|error| error.to_string());
    for done in answers {
        let _ = done.send(committed.clone());
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

pub struct ReadGuard<'a> {
    pub pool: &'a ReadPool,
    pub connection: Option<ReadConnection>,
    /// The connection `Store::read_snapshot` pinned for this thread, shared by every read in it.
    pub pinned: Option<Rc<ReadConnection>>,
}

thread_local! {
    /// While `Store::read_snapshot` runs on this thread: the pool it pinned a connection from,
    /// and that connection, held inside one read transaction.
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
        ReadGuard {
            pool: self,
            connection: Some(connection),
            pinned: None,
        }
    }

    /// Keep `connection` for the next read, or close it when the pool already holds
    /// `max_idle_read_connections()` of them.
    pub fn release(&self, connection: ReadConnection) {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < max_idle_read_connections() {
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
            .map(|connection| &**connection)
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
         PRAGMA query_only = ON;",
    )?;
    connection.pragma_update(None, "cache_size", -(read_cache_kib() as i64))?;
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

    fn test_writer() -> WriterConnection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE events (position INTEGER PRIMARY KEY, label TEXT)")
            .unwrap();
        WriterConnection::new(connection, Arc::new(AtomicU64::new(0)))
    }

    fn enqueue_insert(
        writer: &WriterConnection,
        label: String,
        succeeded: bool,
    ) -> std::sync::mpsc::Receiver<Result<(), String>> {
        let (done, answer) = std::sync::mpsc::sync_channel(1);
        writer.send(WriterJob::Batched {
            run: Box::new(move |tx| {
                tx.execute("INSERT INTO events(label) VALUES (?1)", [&label]).unwrap();
                succeeded
            }),
            profile: None,
            wait: None,
            done,
        });
        answer
    }

    fn await_commit(answer: std::sync::mpsc::Receiver<Result<(), String>>) {
        answer.recv_timeout(std::time::Duration::from_secs(5)).unwrap().unwrap();
    }

    fn events(writer: &WriterConnection) -> Vec<String> {
        let connection = writer.write();
        let mut statement = connection.prepare("SELECT label FROM events ORDER BY position").unwrap();
        statement.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn stopped_writer_drops_queued_captures_before_disconnecting_their_callers() {
        struct Capture(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Capture {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let writer = test_writer();
        let (lent, loan) = std::sync::mpsc::sync_channel(1);
        let (give_back, returned) = std::sync::mpsc::sync_channel(1);
        writer.send(WriterJob::Lend { lent, returned });
        let connection = loan.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let enqueue = || {
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let capture = Capture(dropped.clone());
            let (done, answer) = std::sync::mpsc::sync_channel(1);
            writer.send(WriterJob::Batched {
                run: Box::new(move |_| {
                    drop(capture);
                    panic!("a stopped writer must not run queued jobs");
                }),
                profile: None,
                wait: None,
                done,
            });
            (dropped, answer)
        };
        let ordinary = enqueue();
        let control = with_control_writes(enqueue);
        // Losing a borrowed connection ends the writer, while its enqueue handle stays alive.
        drop(give_back);
        drop(connection);
        for (dropped, answer) in [ordinary, control] {
            assert!(matches!(
                answer.recv_timeout(std::time::Duration::from_secs(5)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ));
            assert!(dropped.load(Ordering::Acquire));
        }
    }

    #[test]
    fn control_arriving_during_an_ordinary_batch_precedes_the_next_connection_loan() {
        let writer = test_writer();
        let (started, running) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let (done, first) = std::sync::mpsc::sync_channel(1);
        writer.send(WriterJob::Batched {
            run: Box::new(move |tx| {
                tx.execute("INSERT INTO events(label) VALUES ('active ordinary')", []).unwrap();
                started.send(()).unwrap();
                wait.recv().unwrap();
                true
            }),
            profile: None,
            wait: None,
            done,
        });
        running.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let (lent, loan) = std::sync::mpsc::sync_channel(1);
        let (give_back, returned) = std::sync::mpsc::sync_channel(1);
        writer.send(WriterJob::Lend { lent, returned });
        let control = with_control_writes(|| enqueue_insert(&writer, "control".into(), true));
        release.send(()).unwrap();
        await_commit(first);
        let connection = loan.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM events WHERE label='control'", [], |row| row.get(0))
            .unwrap();
        give_back.send(connection).unwrap();
        await_commit(control);
        assert_eq!(count, 1, "control must commit before the queued ordinary loan");
        assert_eq!(events(&writer), ["active ordinary", "control"]);
    }

    #[test]
    fn control_write_commits_ahead_of_backlog_without_waiting_for_next_ordinary_write() {
        let writer = test_writer();
        let holder = writer.write();
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let (done, first) = std::sync::mpsc::sync_channel(1);
        writer.send(WriterJob::Batched {
            run: Box::new(move |tx| {
                tx.execute("INSERT INTO events(label) VALUES ('ordinary0')", []).unwrap();
                // The first ordinary job holds its transaction until the test releases it.
                wait.recv().unwrap();
                true
            }),
            profile: None,
            wait: None,
            done,
        });
        let ordinary: Vec<_> = (1..6)
            .map(|index| enqueue_insert(&writer, format!("ordinary{index}"), true))
            .collect();
        let control = with_control_writes(|| enqueue_insert(&writer, "control".into(), true));
        drop(holder);
        let committed = control.recv_timeout(std::time::Duration::from_secs(5));
        // Always release the ordinary writer, even when the priority assertion will fail.
        release.send(()).unwrap();
        await_commit(first);
        for answer in ordinary {
            await_commit(answer);
        }
        committed.unwrap().unwrap();
        assert_eq!(
            events(&writer),
            ["control", "ordinary0", "ordinary1", "ordinary2", "ordinary3", "ordinary4", "ordinary5"]
        );
    }

    #[test]
    fn ordinary_writes_get_a_turn_every_eight_control_turns() {
        let writer = test_writer();
        let holder = writer.write();
        let control: Vec<_> = with_control_writes(|| {
            (0..24)
                .map(|index| enqueue_insert(&writer, format!("control{index}"), true))
                .collect()
        });
        let ordinary0 = enqueue_insert(&writer, "ordinary0".into(), true);
        let ordinary1 = enqueue_insert(&writer, "ordinary1".into(), true);
        drop(holder);
        for answer in control {
            await_commit(answer);
        }
        await_commit(ordinary0);
        await_commit(ordinary1);
        let mut expected = Vec::new();
        for index in 0..24 {
            expected.push(format!("control{index}"));
            if index == 7 {
                expected.push("ordinary0".into());
            } else if index == 15 {
                expected.push("ordinary1".into());
            }
        }
        assert_eq!(events(&writer), expected);
    }

    #[test]
    fn control_connection_loan_passes_ordinary_backlog() {
        let writer = test_writer();
        let holder = writer.write();
        let ordinary = enqueue_insert(&writer, "ordinary".into(), true);
        let (lent, loan) = std::sync::mpsc::sync_channel(1);
        let (give_back, returned) = std::sync::mpsc::sync_channel(1);
        with_control_writes(|| writer.send(WriterJob::Lend { lent, returned }));
        drop(holder);
        let connection = loan.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let count: i64 = connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0)).unwrap();
        connection.execute("INSERT INTO events(label) VALUES ('control loan')", []).unwrap();
        give_back.send(connection).unwrap();
        await_commit(ordinary);
        assert_eq!(count, 0);
        assert_eq!(events(&writer), ["control loan", "ordinary"]);
    }

    #[test]
    fn control_scope_restores_priority_after_nested_and_outer_panics() {
        let writer = test_writer();
        let holder = writer.write();
        let ordinary0 = enqueue_insert(&writer, "ordinary0".into(), true);
        let mut answers = Vec::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_control_writes(|| {
                let nested = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    with_control_writes(|| {
                        answers.push(enqueue_insert(&writer, "control0".into(), true));
                        panic!("nested scope");
                    });
                }));
                assert!(nested.is_err());
                answers.push(enqueue_insert(&writer, "control1".into(), true));
                panic!("outer scope");
            });
        }));
        assert!(panic.is_err());
        let ordinary1 = enqueue_insert(&writer, "ordinary1".into(), true);
        drop(holder);
        for answer in answers {
            await_commit(answer);
        }
        await_commit(ordinary0);
        await_commit(ordinary1);
        assert_eq!(events(&writer), ["control0", "control1", "ordinary0", "ordinary1"]);
    }

    #[test]
    fn default_writes_remain_fifo_and_rollback_only_the_failed_savepoint() {
        let writer = test_writer();
        let holder = writer.write();
        let first = enqueue_insert(&writer, "first".into(), true);
        let failed = enqueue_insert(&writer, "rolled back".into(), false);
        let last = enqueue_insert(&writer, "last".into(), true);
        drop(holder);
        await_commit(first);
        await_commit(failed);
        await_commit(last);
        assert_eq!(events(&writer), ["first", "last"]);
    }

    #[test]
    fn control_batched_writes_finish_borrows_and_roll_back_errors_and_panics() {
        let writer = test_writer();
        let label = String::from("borrowed");
        let mut ran = false;
        let result = with_control_writes(|| writer.batched(|tx| -> rusqlite::Result<()> {
            tx.execute("INSERT INTO events(label) VALUES (?1)", [&label])?;
            ran = true;
            Ok(())
        }));
        result.unwrap().unwrap();
        assert!(ran);
        let failed = with_control_writes(|| writer.batched(|tx| -> rusqlite::Result<()> {
            tx.execute("INSERT INTO events(label) VALUES ('error')", [])?;
            Err(rusqlite::Error::InvalidQuery)
        }));
        assert!(matches!(failed, Ok(Err(rusqlite::Error::InvalidQuery))));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_control_writes(|| writer.batched(|tx| -> rusqlite::Result<()> {
                tx.execute("INSERT INTO events(label) VALUES ('panic')", [])?;
                panic!("borrowed job");
            }))
        }));
        assert!(panic.is_err());
        assert_eq!(events(&writer), ["borrowed"]);
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
