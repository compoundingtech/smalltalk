//! Opt-in accounting of where the daemon's time goes.
//!
//! Set `ST3_PROFILE_DIR` to a directory before `st3 up` and the daemon records, for each request
//! route and background task: how long it queued before a thread ran it, how long it waited for
//! the store's writer and read connections and which operation held the writer meanwhile, how
//! long its SQLite statements ran and which ones, and how much CPU its threads used. Every minute
//! it appends one line to `minutes.jsonl`; each operation slower than `ST3_PROFILE_SLOW_MS`
//! (default 250) appends one line to `slow.jsonl`. Unset, every hook is one relaxed atomic load.
//!
//! SQLite reports a statement's time from its first step to its reset, so a query whose rows the
//! caller processes one by one includes that processing. Statement time is an upper bound on time
//! inside SQLite, and concurrent operations' statement times overlap.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<State> = OnceLock::new();

/// Wall time of one operation, bucketed by these upper bounds in milliseconds.
const WALL_BUCKETS_MS: [u64; 8] = [10, 50, 100, 250, 1_000, 5_000, 15_000, u64::MAX];
/// Distinct statements kept per operation label. Later ones fold into `(other statements)`.
const STATEMENT_LIMIT: usize = 400;
/// Wall-time samples kept per label per minute for percentiles.
const MINUTE_SAMPLES: usize = 4096;

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn profiling on when `ST3_PROFILE_DIR` names a directory. Call once, before the store opens.
pub fn init_from_env() {
    let Some(dir) = std::env::var_os("ST3_PROFILE_DIR").map(PathBuf::from) else {
        return;
    };
    if let Err(error) = fs::create_dir_all(&dir) {
        eprintln!("st3: profiling is off: create {}: {error}", dir.display());
        return;
    }
    let slow_after = std::env::var("ST3_PROFILE_SLOW_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(250));
    let slow = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("slow.jsonl"))
        .ok();
    let state = State {
        dir,
        slow_after,
        minute: Mutex::new(Minute::default()),
        total: Mutex::new(BTreeMap::new()),
        unlabeled: Mutex::new(HashMap::new()),
        writer_holder: Mutex::new(None),
        thread_labels: Mutex::new(HashMap::new()),
        slow: Mutex::new(slow),
    };
    if STATE.set(state).is_err() {
        return;
    }
    // Hosts that allow tracing only a process's own descendants can still sample this one
    // with a stack sampler while the profile runs.
    #[cfg(target_os = "linux")]
    if std::env::var_os("ST3_PROFILE_PTRACER").is_some() {
        // SAFETY: PR_SET_PTRACER only changes which processes may trace this one.
        unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0) };
    }
    ENABLED.store(true, Ordering::Relaxed);
    let _ = std::thread::Builder::new()
        .name("st3-profile".into())
        .spawn(flush_loop);
    eprintln!(
        "st3: profiling into {} (slow after {} ms)",
        STATE.get().expect("just set").dir.display(),
        STATE.get().expect("just set").slow_after.as_millis()
    );
}

/// Measure how late the async runtime wakes a timer, the delay every async task sees.
pub async fn watch_runtime_lag() {
    if !enabled() {
        return;
    }
    let period = Duration::from_millis(100);
    loop {
        let started = Instant::now();
        tokio::time::sleep(period).await;
        let lag = started.elapsed().saturating_sub(period);
        if let Some(state) = STATE.get() {
            let mut minute = state.minute.lock().unwrap_or_else(PoisonError::into_inner);
            minute.lag.record(lag);
        }
    }
}

struct State {
    dir: PathBuf,
    slow_after: Duration,
    minute: Mutex<Minute>,
    total: Mutex<BTreeMap<Arc<str>, Totals>>,
    /// Work on a thread that runs no labelled operation, by thread name.
    unlabeled: Mutex<HashMap<String, Acc>>,
    /// The label of the operation that holds the store's writer.
    writer_holder: Mutex<Option<Arc<str>>>,
    /// The label each thread runs now, for attributing sampled thread CPU.
    thread_labels: Mutex<HashMap<i32, Arc<str>>>,
    slow: Mutex<Option<File>>,
}

#[derive(Default)]
struct Minute {
    ops: BTreeMap<Arc<str>, Totals>,
    callers: BTreeMap<(Arc<str>, Arc<str>), CallerTotals>,
    lag: Lag,
}

#[derive(Default)]
struct Lag {
    ticks: u64,
    over_10ms: u64,
    over_100ms: u64,
    over_1s: u64,
    total_ns: u64,
    max_ns: u64,
}

impl Lag {
    fn record(&mut self, lag: Duration) {
        let ns = nanos(lag);
        self.ticks += 1;
        self.total_ns += ns;
        self.max_ns = self.max_ns.max(ns);
        if lag >= Duration::from_millis(10) {
            self.over_10ms += 1;
        }
        if lag >= Duration::from_millis(100) {
            self.over_100ms += 1;
        }
        if lag >= Duration::from_secs(1) {
            self.over_1s += 1;
        }
    }
}

#[derive(Default)]
struct CallerTotals {
    count: u64,
    wall_ns: u64,
    cpu_ns: u64,
}

/// A thread's `/proc/thread-self/io` counters.
#[derive(Clone, Copy, Default)]
struct ThreadIo {
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
}

impl ThreadIo {
    #[cfg(target_os = "linux")]
    fn now() -> Self {
        let io = fs::read_to_string("/proc/thread-self/io").unwrap_or_default();
        let field = |name: &str| {
            io.lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or_default()
        };
        Self {
            rchar: field("rchar:"),
            wchar: field("wchar:"),
            read_bytes: field("read_bytes:"),
            write_bytes: field("write_bytes:"),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn now() -> Self {
        Self::default()
    }

    fn since(self, started: Self) -> Self {
        Self {
            rchar: self.rchar.saturating_sub(started.rchar),
            wchar: self.wchar.saturating_sub(started.wchar),
            read_bytes: self.read_bytes.saturating_sub(started.read_bytes),
            write_bytes: self.write_bytes.saturating_sub(started.write_bytes),
        }
    }

    fn add(&mut self, other: Self) {
        self.rchar += other.rchar;
        self.wchar += other.wchar;
        self.read_bytes += other.read_bytes;
        self.write_bytes += other.write_bytes;
    }
}

#[derive(Clone, Copy, Default)]
struct Stat {
    count: u64,
    ns: u64,
    max_ns: u64,
}

impl Stat {
    fn add(&mut self, ns: u64) {
        self.count += 1;
        self.ns += ns;
        self.max_ns = self.max_ns.max(ns);
    }

    fn merge(&mut self, other: &Stat) {
        self.count += other.count;
        self.ns += other.ns;
        self.max_ns = self.max_ns.max(other.max_ns);
    }
}

/// What one operation, or one thread's unlabelled work, spent.
#[derive(Default)]
struct Acc {
    queue_ns: u64,
    envelope_ns: u64,
    response_bytes: u64,
    cpu_ns: u64,
    /// Bytes this operation's threads read and wrote through the kernel, and the part of that
    /// which reached the block device.
    io: ThreadIo,
    sql: Stat,
    writer_wait: Stat,
    writer_hold: Stat,
    read_wait: Stat,
    /// Writer wait by the label that held the writer when the wait began.
    blocked_by: HashMap<Arc<str>, u64>,
    statements: HashMap<Box<str>, Stat>,
    /// Named parts of the operation: wall, CPU and statement time.
    spans: HashMap<Box<str>, SpanStat>,
    /// Counted events, such as why a projection replayed the graph.
    notes: HashMap<Box<str>, u64>,
}

#[derive(Clone, Copy, Default)]
struct SpanStat {
    count: u64,
    wall_ns: u64,
    max_wall_ns: u64,
    cpu_ns: u64,
    sql_ns: u64,
}

impl Acc {
    fn statement(&mut self, statement: &str, ns: u64) {
        self.sql.add(ns);
        if let Some(stat) = self.statements.get_mut(statement) {
            stat.add(ns);
        } else if self.statements.len() < STATEMENT_LIMIT {
            let mut stat = Stat::default();
            stat.add(ns);
            self.statements.insert(statement.into(), stat);
        } else {
            self.statements
                .entry("(other statements)".into())
                .or_default()
                .add(ns);
        }
    }
}

#[derive(Default)]
struct Totals {
    count: u64,
    wall: Stat,
    wall_buckets: [u64; WALL_BUCKETS_MS.len()],
    wall_samples: Vec<u64>,
    acc: Acc,
    sampled_cpu_ns: u64,
}

impl Totals {
    fn merge(&mut self, wall: Option<u64>, acc: &Acc) {
        if let Some(wall) = wall {
            self.count += 1;
            self.wall.add(wall);
            let ms = wall / 1_000_000;
            let bucket = WALL_BUCKETS_MS
                .iter()
                .position(|bound| ms < *bound)
                .unwrap_or(WALL_BUCKETS_MS.len() - 1);
            self.wall_buckets[bucket] += 1;
            if self.wall_samples.len() < MINUTE_SAMPLES {
                self.wall_samples.push(wall);
            }
        }
        let into = &mut self.acc;
        into.queue_ns += acc.queue_ns;
        into.envelope_ns += acc.envelope_ns;
        into.response_bytes += acc.response_bytes;
        into.cpu_ns += acc.cpu_ns;
        into.io.add(acc.io);
        into.sql.merge(&acc.sql);
        into.writer_wait.merge(&acc.writer_wait);
        into.writer_hold.merge(&acc.writer_hold);
        into.read_wait.merge(&acc.read_wait);
        for (holder, ns) in &acc.blocked_by {
            *into.blocked_by.entry(holder.clone()).or_default() += ns;
        }
        for (name, span) in &acc.spans {
            let into = into.spans.entry(name.clone()).or_default();
            into.count += span.count;
            into.wall_ns += span.wall_ns;
            into.max_wall_ns = into.max_wall_ns.max(span.max_wall_ns);
            into.cpu_ns += span.cpu_ns;
            into.sql_ns += span.sql_ns;
        }
        for (note, count) in &acc.notes {
            *into.notes.entry(note.clone()).or_default() += count;
        }
        for (statement, stat) in &acc.statements {
            if let Some(existing) = into.statements.get_mut(statement) {
                existing.merge(stat);
            } else if into.statements.len() < STATEMENT_LIMIT {
                into.statements.insert(statement.clone(), *stat);
            } else {
                into.statements
                    .entry("(other statements)".into())
                    .or_default()
                    .merge(stat);
            }
        }
    }
}

struct OpInner {
    label: Arc<str>,
    caller: Option<Arc<str>>,
    started: Instant,
    acc: Mutex<Acc>,
}

/// Who sent a request: the harness it belongs to, if any, and the command that sent it.
#[derive(Clone)]
pub struct Caller(pub Arc<str>);

impl Caller {
    /// Name a peer by its first command words (`None` once it exited), under its bound agent.
    pub fn of_command(arguments: Option<Vec<String>>, bound_agent: Option<&str>) -> Self {
        let command = arguments
            .map(|words| {
                let program = words
                    .first()
                    .map(|program| program.rsplit('/').next().unwrap_or(program).to_owned())
                    .unwrap_or_default();
                // The first two subcommand words: plain lowercase words that follow no `--flag`.
                let subcommand = words
                    .iter()
                    .enumerate()
                    .skip(1)
                    .filter(|(index, word)| {
                        !word.is_empty()
                            && word
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
                            && !word.starts_with('-')
                            && !(words[index - 1].starts_with("--")
                                && !words[index - 1].contains('='))
                    })
                    .map(|(_, word)| word.clone())
                    .take(2)
                    .collect::<Vec<_>>();
                if subcommand.is_empty() {
                    program
                } else {
                    format!("{program} {}", subcommand.join(" "))
                }
            })
            .unwrap_or_else(|| "(exited)".into());
        Caller(match bound_agent {
            Some(agent) => format!("{agent} · {command}").into(),
            None => command.into(),
        })
    }
}

/// One request or background pass. Threads that work for it enter it; `finish` records it.
#[derive(Clone)]
pub struct Op(Arc<OpInner>);

thread_local! {
    static CURRENT: RefCell<Option<Arc<OpInner>>> = const { RefCell::new(None) };
    static THREAD_ID: i32 = thread_id();
}

/// Leaves the operation a thread entered, adding the thread's CPU time and I/O to it.
pub struct Entered {
    op: Option<Arc<OpInner>>,
    cpu_started: u64,
    io_started: ThreadIo,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let Some(op) = self.op.take() else {
            return;
        };
        let cpu = thread_cpu_ns().saturating_sub(self.cpu_started);
        let io = ThreadIo::now().since(self.io_started);
        {
            let mut acc = op.acc.lock().unwrap_or_else(PoisonError::into_inner);
            acc.cpu_ns += cpu;
            acc.io.add(io);
        }
        CURRENT.with(|current| current.borrow_mut().take());
        if let Some(state) = STATE.get() {
            let tid = THREAD_ID.with(|tid| *tid);
            state
                .thread_labels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&tid);
        }
    }
}

impl Op {
    /// Start an operation, or nothing when profiling is off.
    pub fn start(label: impl Into<Arc<str>>, caller: Option<Arc<str>>) -> Option<Op> {
        enabled().then(|| {
            Op(Arc::new(OpInner {
                label: label.into(),
                caller,
                started: Instant::now(),
                acc: Mutex::new(Acc::default()),
            }))
        })
    }

    /// Run the rest of this thread's work for this operation until the guard drops. A thread
    /// already working for an operation keeps it.
    pub fn enter(&self) -> Entered {
        let entered = CURRENT.with(|current| {
            let mut current = current.borrow_mut();
            if current.is_some() {
                return false;
            }
            *current = Some(self.0.clone());
            true
        });
        if !entered {
            return Entered {
                op: None,
                cpu_started: 0,
                io_started: ThreadIo::default(),
            };
        }
        if let Some(state) = STATE.get() {
            let tid = THREAD_ID.with(|tid| *tid);
            state
                .thread_labels
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(tid, self.0.label.clone());
        }
        Entered {
            op: Some(self.0.clone()),
            cpu_started: thread_cpu_ns(),
            io_started: ThreadIo::now(),
        }
    }

    /// Time from the operation's start until a thread began its work.
    pub fn queued(&self) {
        let queued = nanos(self.0.started.elapsed());
        self.0
            .acc
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queue_ns = queued;
    }

    /// Time spent wrapping the handler's response, and the response's size.
    pub fn enveloped(&self, envelope: Duration, bytes: usize) {
        let mut acc = self.0.acc.lock().unwrap_or_else(PoisonError::into_inner);
        acc.envelope_ns += nanos(envelope);
        acc.response_bytes += bytes as u64;
    }

    pub fn finish(self) {
        let Some(state) = STATE.get() else {
            return;
        };
        let wall = self.0.started.elapsed();
        let acc = std::mem::take(&mut *self.0.acc.lock().unwrap_or_else(PoisonError::into_inner));
        if wall >= state.slow_after {
            write_slow(state, &self.0.label, self.0.caller.as_deref(), wall, &acc);
        }
        let wall_ns = nanos(wall);
        {
            let mut minute = state.minute.lock().unwrap_or_else(PoisonError::into_inner);
            minute
                .ops
                .entry(self.0.label.clone())
                .or_default()
                .merge(Some(wall_ns), &acc);
            let caller = self.0.caller.clone().unwrap_or_else(|| "(daemon)".into());
            let entry = minute
                .callers
                .entry((caller, self.0.label.clone()))
                .or_default();
            entry.count += 1;
            entry.wall_ns += wall_ns;
            entry.cpu_ns += acc.cpu_ns;
        }
        let mut total = state.total.lock().unwrap_or_else(PoisonError::into_inner);
        let totals = total.entry(self.0.label.clone()).or_default();
        totals.merge(Some(wall_ns), &acc);
        totals.wall_samples.clear();
    }
}

/// The operation this thread works for, to hand to another thread.
pub fn current() -> Option<Op> {
    if !enabled() {
        return None;
    }
    CURRENT.with(|current| current.borrow().clone().map(Op))
}

/// Enter `op` on this thread, when there is one.
pub fn enter(op: Option<&Op>) -> Option<Entered> {
    op.map(Op::enter)
}

/// Run `work` as its own operation, unless this thread already works for one.
pub fn task<T>(label: &'static str, work: impl FnOnce() -> T) -> T {
    crate::performance::task(label, || profiled_task(label, work))
}

fn profiled_task<T>(label: &'static str, work: impl FnOnce() -> T) -> T {
    if !enabled() || CURRENT.with(|current| current.borrow().is_some()) {
        return work();
    }
    let Some(op) = Op::start(label, None) else {
        return work();
    };
    let result = {
        let _entered = op.enter();
        work()
    };
    op.finish();
    result
}

fn with_acc_or_unlabeled(record: impl Fn(&mut Acc)) {
    let recorded = CURRENT.with(|current| {
        let current = current.borrow();
        let op = current.as_ref()?;
        record(&mut op.acc.lock().unwrap_or_else(PoisonError::into_inner));
        Some(())
    });
    if recorded.is_some() {
        return;
    }
    let Some(state) = STATE.get() else {
        return;
    };
    let name = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    let mut unlabeled = state
        .unlabeled
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    record(unlabeled.entry(name).or_default());
}

fn current_label() -> Arc<str> {
    CURRENT
        .with(|current| current.borrow().as_ref().map(|op| op.label.clone()))
        .unwrap_or_else(|| {
            format!(
                "(unlabeled {})",
                std::thread::current().name().unwrap_or("unnamed")
            )
            .into()
        })
}

/// Count `note` on this thread's operation, such as why it took a slow path.
pub fn note(note: &str) {
    if !enabled() {
        return;
    }
    with_acc_or_unlabeled(|acc| {
        if let Some(count) = acc.notes.get_mut(note) {
            *count += 1;
        } else if acc.notes.len() < STATEMENT_LIMIT {
            acc.notes.insert(note.into(), 1);
        }
    });
}

/// Time a named part of this thread's operation until the guard drops.
pub struct Span {
    name: Box<str>,
    started: Instant,
    cpu_started: u64,
    sql_started: u64,
}

pub fn span(name: &str) -> Option<Span> {
    if !enabled() || CURRENT.with(|current| current.borrow().is_none()) {
        return None;
    }
    let sql_started = CURRENT.with(|current| {
        current.borrow().as_ref().map_or(0, |op| {
            op.acc.lock().unwrap_or_else(PoisonError::into_inner).sql.ns
        })
    });
    Some(Span {
        name: name.into(),
        started: Instant::now(),
        cpu_started: thread_cpu_ns(),
        sql_started,
    })
}

impl Drop for Span {
    fn drop(&mut self) {
        let wall = nanos(self.started.elapsed());
        let cpu = thread_cpu_ns().saturating_sub(self.cpu_started);
        CURRENT.with(|current| {
            let current = current.borrow();
            let Some(op) = current.as_ref() else {
                return;
            };
            let mut acc = op.acc.lock().unwrap_or_else(PoisonError::into_inner);
            let sql = acc.sql.ns.saturating_sub(self.sql_started);
            let span = acc.spans.entry(self.name.clone()).or_default();
            span.count += 1;
            span.wall_ns += wall;
            span.max_wall_ns = span.max_wall_ns.max(wall);
            span.cpu_ns += cpu;
            span.sql_ns += sql;
        });
    }
}

/// A SQLite statement finished after `duration`.
pub fn sql(statement: &str, duration: Duration) {
    if !enabled() {
        return;
    }
    let ns = nanos(duration);
    with_acc_or_unlabeled(|acc| acc.statement(statement, ns));
}

/// Before waiting for the store's writer: when, and who holds it.
pub struct WriterWait {
    started: Instant,
    holder: Option<Arc<str>>,
}

pub fn writer_waiting() -> Option<WriterWait> {
    let state = STATE.get().filter(|_| enabled())?;
    Some(WriterWait {
        started: Instant::now(),
        holder: state
            .writer_holder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    })
}

/// The writer is this thread's now. Returns when it was taken, for `writer_released`.
pub fn writer_acquired(wait: Option<WriterWait>) -> Option<Instant> {
    let wait = wait?;
    let state = STATE.get()?;
    let waited = nanos(wait.started.elapsed());
    *state
        .writer_holder
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(current_label());
    let holder = wait.holder;
    with_acc_or_unlabeled(|acc| {
        acc.writer_wait.add(waited);
        if waited >= 1_000_000
            && let Some(holder) = &holder
        {
            *acc.blocked_by.entry(holder.clone()).or_default() += waited;
        }
    });
    Some(Instant::now())
}

pub fn writer_released(acquired: Option<Instant>) {
    let Some(acquired) = acquired else {
        return;
    };
    let held = nanos(acquired.elapsed());
    if let Some(state) = STATE.get() {
        *state
            .writer_holder
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
    with_acc_or_unlabeled(|acc| acc.writer_hold.add(held));
}

/// This thread waited `waited` for a pooled read connection or a read snapshot slot.
pub fn read_waited(waited: Duration) {
    if !enabled() {
        return;
    }
    let ns = nanos(waited);
    with_acc_or_unlabeled(|acc| acc.read_wait.add(ns));
}

#[cfg(target_os = "linux")]
fn thread_id() -> i32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::gettid() }
}

#[cfg(not(target_os = "linux"))]
fn thread_id() -> i32 {
    0
}

fn thread_cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid timespec for the call to fill.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn ms(ns: u64) -> f64 {
    (ns as f64 / 1_000_000.0 * 10.0).round() / 10.0
}

fn mb(bytes: u64) -> f64 {
    (bytes as f64 / 100_000.0).round() / 10.0
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn compact_sql(statement: &str) -> String {
    let mut compact = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() > 240 {
        let mut end = 240;
        while !compact.is_char_boundary(end) {
            end -= 1;
        }
        compact.truncate(end);
        compact.push('…');
    }
    compact
}

fn top_statements(acc: &Acc, limit: usize) -> Vec<Value> {
    let mut statements = acc.statements.iter().collect::<Vec<_>>();
    statements.sort_by_key(|(_, stat)| std::cmp::Reverse(stat.ns));
    statements
        .into_iter()
        .take(limit)
        .map(|(statement, stat)| {
            json!({
                "sql": compact_sql(statement),
                "count": stat.count,
                "ms": ms(stat.ns),
                "max_ms": ms(stat.max_ns),
            })
        })
        .collect()
}

fn spans_json(acc: &Acc) -> Vec<Value> {
    let mut spans = acc.spans.iter().collect::<Vec<_>>();
    spans.sort_by_key(|(_, span)| std::cmp::Reverse(span.wall_ns));
    spans
        .into_iter()
        .take(30)
        .map(|(name, span)| {
            json!({
                "name": name.as_ref(),
                "count": span.count,
                "wall_ms": ms(span.wall_ns),
                "max_ms": ms(span.max_wall_ns),
                "cpu_ms": ms(span.cpu_ns),
                "sql_ms": ms(span.sql_ns),
            })
        })
        .collect()
}

fn top_blockers(acc: &Acc, limit: usize) -> Vec<Value> {
    let mut blockers = acc.blocked_by.iter().collect::<Vec<_>>();
    blockers.sort_by_key(|(_, ns)| std::cmp::Reverse(**ns));
    blockers
        .into_iter()
        .take(limit)
        .map(|(holder, ns)| json!({"holder": holder.as_ref(), "ms": ms(*ns)}))
        .collect()
}

fn acc_json(acc: &Acc, statements: usize) -> Value {
    json!({
        "queue_ms": ms(acc.queue_ns),
        "envelope_ms": ms(acc.envelope_ns),
        "response_bytes": acc.response_bytes,
        "cpu_ms": ms(acc.cpu_ns),
        "io_read_mb": mb(acc.io.rchar),
        "io_written_mb": mb(acc.io.wchar),
        "disk_read_mb": mb(acc.io.read_bytes),
        "disk_written_mb": mb(acc.io.write_bytes),
        "sql_ms": ms(acc.sql.ns),
        "sql_count": acc.sql.count,
        "sql_max_ms": ms(acc.sql.max_ns),
        "writer_wait_ms": ms(acc.writer_wait.ns),
        "writer_wait_max_ms": ms(acc.writer_wait.max_ns),
        "writer_acquisitions": acc.writer_wait.count,
        "writer_hold_ms": ms(acc.writer_hold.ns),
        "writer_hold_max_ms": ms(acc.writer_hold.max_ns),
        "read_wait_ms": ms(acc.read_wait.ns),
        "read_wait_max_ms": ms(acc.read_wait.max_ns),
        "read_acquisitions": acc.read_wait.count,
        "blocked_by": top_blockers(acc, 5),
        "statements": top_statements(acc, statements),
        "spans": spans_json(acc),
        "notes": acc.notes.iter().map(|(note, count)| (note.to_string(), Value::from(*count))).collect::<serde_json::Map<_, _>>(),
    })
}

fn write_slow(state: &State, label: &str, caller: Option<&str>, wall: Duration, acc: &Acc) {
    let mut line = json!({
        "at_unix_ms": unix_ms(),
        "label": label,
        "caller": caller,
        "wall_ms": ms(nanos(wall)),
    });
    if let (Value::Object(line), Value::Object(detail)) = (&mut line, acc_json(acc, 6)) {
        line.extend(detail);
    }
    let mut slow = state.slow.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(file) = slow.as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * percent).div_ceil(100).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

fn totals_json(label: &str, totals: &mut Totals, statements: usize) -> Value {
    totals.wall_samples.sort_unstable();
    let mut value = json!({
        "label": label,
        "count": totals.count,
        "wall_ms": ms(totals.wall.ns),
        "wall_max_ms": ms(totals.wall.max_ns),
        "p50_ms": ms(percentile(&totals.wall_samples, 50)),
        "p90_ms": ms(percentile(&totals.wall_samples, 90)),
        "p99_ms": ms(percentile(&totals.wall_samples, 99)),
        "wall_buckets": WALL_BUCKETS_MS
            .iter()
            .zip(totals.wall_buckets)
            .filter(|(_, count)| *count > 0)
            .map(|(bound, count)| {
                let bound = if *bound == u64::MAX { "inf".to_owned() } else { bound.to_string() };
                (format!("lt_{bound}ms"), Value::from(count))
            })
            .collect::<serde_json::Map<_, _>>(),
        "sampled_cpu_ms": ms(totals.sampled_cpu_ns),
    });
    if let (Value::Object(value), Value::Object(detail)) =
        (&mut value, acc_json(&totals.acc, statements))
    {
        value.extend(detail);
    }
    value
}

/// Per-thread CPU ticks from `/proc/self/task`, by thread ID, with each thread's name.
fn thread_ticks() -> HashMap<i32, (u64, String)> {
    let mut threads = HashMap::new();
    let Ok(tasks) = fs::read_dir("/proc/self/task") else {
        return threads;
    };
    for task in tasks.flatten() {
        let Some(tid) = task.file_name().to_str().and_then(|tid| tid.parse().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(task.path().join("stat")) else {
            continue;
        };
        let Some((head, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let name = head.split_once(" (").map(|(_, name)| name).unwrap_or("");
        let fields = rest.split_whitespace().collect::<Vec<_>>();
        // After the name: state is field 3, utime field 14 and stime field 15 of the whole line.
        let ticks = fields
            .get(11)
            .and_then(|utime| utime.parse::<u64>().ok())
            .unwrap_or_default()
            + fields
                .get(12)
                .and_then(|stime| stime.parse::<u64>().ok())
                .unwrap_or_default();
        threads.insert(tid, (ticks, name.to_owned()));
    }
    threads
}

fn process_status() -> Value {
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields = stat
        .rsplit_once(") ")
        .map(|(_, rest)| rest.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();
    let field = |index: usize| {
        fields
            .get(index)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
    };
    let io = fs::read_to_string("/proc/self/io").unwrap_or_default();
    let io_field = |name: &str| {
        io.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_default()
    };
    json!({
        "cpu_ticks": field(11) + field(12),
        "threads": field(17),
        "rss_pages": field(21),
        "rchar": io_field("rchar:"),
        "wchar": io_field("wchar:"),
        "read_bytes": io_field("read_bytes:"),
        "write_bytes": io_field("write_bytes:"),
    })
}

fn flush_loop() {
    let Some(state) = STATE.get() else {
        return;
    };
    let mut previous_ticks = thread_ticks();
    let mut last_flush = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        // Sample each thread's CPU and charge it to what that thread runs now.
        let ticks = thread_ticks();
        let labels = state
            .thread_labels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut sampled = HashMap::<Arc<str>, u64>::new();
        for (tid, (now, name)) in &ticks {
            let before = previous_ticks
                .get(tid)
                .map(|(ticks, _)| *ticks)
                .unwrap_or(0);
            let delta = now.saturating_sub(before);
            if delta == 0 {
                continue;
            }
            let label = labels
                .get(tid)
                .cloned()
                .unwrap_or_else(|| format!("(thread {name})").into());
            *sampled.entry(label).or_default() += delta * 10_000_000;
        }
        previous_ticks = ticks;
        {
            let mut minute = state.minute.lock().unwrap_or_else(PoisonError::into_inner);
            for (label, ns) in sampled {
                minute.ops.entry(label).or_default().sampled_cpu_ns += ns;
            }
        }
        if last_flush.elapsed() < Duration::from_secs(60) {
            continue;
        }
        last_flush = Instant::now();
        flush_minute(state);
    }
}

fn flush_minute(state: &State) {
    let mut minute =
        std::mem::take(&mut *state.minute.lock().unwrap_or_else(PoisonError::into_inner));
    let unlabeled = std::mem::take(
        &mut *state
            .unlabeled
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    for (name, acc) in unlabeled {
        let label: Arc<str> = format!("(unlabeled {name})").into();
        minute
            .ops
            .entry(label.clone())
            .or_default()
            .merge(None, &acc);
        state
            .total
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(label)
            .or_default()
            .merge(None, &acc);
    }
    let mut ops = minute
        .ops
        .iter_mut()
        .map(|(label, totals)| totals_json(label, totals, 5))
        .collect::<Vec<_>>();
    let weight = |op: &Value| {
        op["cpu_ms"]
            .as_f64()
            .unwrap_or_default()
            .max(op["sampled_cpu_ms"].as_f64().unwrap_or_default())
            + op["writer_hold_ms"].as_f64().unwrap_or_default()
            + op["wall_ms"].as_f64().unwrap_or_default() / 10.0
    };
    ops.sort_by(|left, right| weight(right).total_cmp(&weight(left)));
    let mut callers = minute
        .callers
        .iter()
        .map(|((caller, label), totals)| {
            json!({
                "caller": caller.as_ref(),
                "label": label.as_ref(),
                "count": totals.count,
                "wall_ms": ms(totals.wall_ns),
                "cpu_ms": ms(totals.cpu_ns),
            })
        })
        .collect::<Vec<_>>();
    callers.sort_by_key(|caller| std::cmp::Reverse(caller["count"].as_u64().unwrap_or_default()));
    callers.truncate(200);
    let lag = &minute.lag;
    let line = json!({
        "at_unix_ms": unix_ms(),
        "process": process_status(),
        "runtime_lag": {
            "ticks": lag.ticks,
            "over_10ms": lag.over_10ms,
            "over_100ms": lag.over_100ms,
            "over_1s": lag.over_1s,
            "mean_ms": ms(lag.total_ns.checked_div(lag.ticks).unwrap_or_default()),
            "max_ms": ms(lag.max_ns),
        },
        "ops": ops,
        "callers": callers,
    });
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.dir.join("minutes.jsonl"))
    {
        let _ = writeln!(file, "{line}");
    }
    let mut total = state.total.lock().unwrap_or_else(PoisonError::into_inner);
    let totals = total
        .iter_mut()
        .map(|(label, totals)| totals_json(label, totals, 40))
        .collect::<Vec<_>>();
    drop(total);
    let path = state.dir.join("totals.json");
    let temporary = state.dir.join("totals.json.tmp");
    if fs::write(
        &temporary,
        serde_json::to_vec(&json!({"at_unix_ms": unix_ms(), "ops": totals})).unwrap_or_default(),
    )
    .is_ok()
    {
        let _ = fs::rename(temporary, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_map_folds_past_its_limit() {
        let mut acc = Acc::default();
        for index in 0..STATEMENT_LIMIT + 3 {
            acc.statement(&format!("SELECT {index}"), 1_000);
        }
        assert_eq!(acc.statements.len(), STATEMENT_LIMIT + 1);
        assert_eq!(acc.statements["(other statements)"].count, 3);
        assert_eq!(acc.sql.count, (STATEMENT_LIMIT + 3) as u64);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples = (1..=100).collect::<Vec<u64>>();
        assert_eq!(percentile(&samples, 50), 50);
        assert_eq!(percentile(&samples, 99), 99);
        assert_eq!(percentile(&[], 99), 0);
    }

    #[test]
    fn compact_sql_collapses_whitespace() {
        assert_eq!(compact_sql("SELECT a\n     FROM b"), "SELECT a FROM b");
    }
}
