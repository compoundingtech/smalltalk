//! Who held the store's single writer, and who kept a read transaction open, and for how long.
//!
//! Always on and bounded, like the windows beside it. A holder is the request route or background
//! task its thread worked for when it asked (`performance::current`), with the client that sent
//! the request, or the thread's name when it worked for neither, and always the code location
//! that asked. The record keeps each writer's hold in progress (a daemon has one writer per
//! store), each holder's writer holds over the last hour in five-minute slots, and the last 64
//! writer holds and 64 read transactions over their target. A read under its target costs one
//! clock read and is not attributed.
//!
//! A lend asked for inside `Stages::begin` also says where its hold went: each stage its borrower
//! entered, and what no stage covers, which is the handoff between the writer thread and the
//! borrower. Other holds carry no stages. Stages never overlap: entering one, including through
//! `stage` from code the borrower calls, pauses the one it interrupts, so the stages and the
//! unattributed rest add up to the hold. A staged lend also counts the aggregates it rebuilt and
//! their claims, by the stage that rebuilt them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::StoreWork;

/// Distinct holders kept; later ones share `(other)`.
const HOLDERS: usize = 64;
/// Long holds and long reads kept, newest last.
const LONG: usize = 64;
const SLOT_SECONDS: u64 = 300;
const SLOTS: u64 = 12;
const HOUR: Duration = Duration::from_secs(SLOT_SECONDS * SLOTS);
/// Holders reported, by total hold time over the hour.
const REPORTED: usize = 16;

/// Who asked for the writer or a read transaction. One pointer, so a queued write stays small.
#[derive(Clone, Debug)]
pub struct Holder(Arc<Asked>);

#[derive(Debug)]
struct Asked {
    /// A request route or a background task, or `(thread NAME)`.
    label: Arc<str>,
    /// The client that sent the request, when known.
    client: Option<Arc<str>>,
    at: &'static Location<'static>,
    /// The `longest_during` scope this thread was in when it asked.
    observer: Option<Arc<AtomicU64>>,
    /// The `Stages` scope this thread was in when it asked.
    stages: Option<Arc<Mutex<StageClock>>>,
}

impl Holder {
    /// The work this thread does now, asked for at the caller's location.
    #[track_caller]
    pub fn current() -> Self {
        Self::asked_at(Location::caller())
    }

    fn asked_at(at: &'static Location<'static>) -> Self {
        let (label, client) = match crate::performance::current() {
            Some(charged) => (charged.kind().into(), charged.client().map(Into::into)),
            None => (
                format!("(thread {})", std::thread::current().name().unwrap_or("unnamed")).into(),
                None,
            ),
        };
        let observer = OBSERVER.with(|observer| observer.borrow().clone());
        let stages = STAGES.with(|stages| stages.borrow().clone());
        Self(Arc::new(Asked {
            label,
            client,
            at,
            observer,
            stages,
        }))
    }

    fn json(&self) -> Value {
        json!({
            "holder": &*self.0.label,
            "client": self.0.client.as_deref(),
            "at": format!("{}:{}", self.0.at.file(), self.0.at.line()),
        })
    }
}

/// How the writer was held: one group-commit batch, or a connection lent to one caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldKind {
    Batch,
    Lend,
}

impl HoldKind {
    fn name(self) -> &'static str {
        match self {
            HoldKind::Batch => "batch",
            HoldKind::Lend => "lend",
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Slot {
    epoch: u64,
    count: u64,
    over: u64,
    total_us: u64,
    max_us: u64,
}

struct Current {
    holder: Holder,
    kind: HoldKind,
    started: Instant,
    writes: usize,
}

impl Current {
    /// The stage a staged lend's borrower is in now.
    fn stage(&self) -> Option<&'static str> {
        if self.kind != HoldKind::Lend {
            return None;
        }
        let clock = self.holder.0.stages.as_ref()?;
        clock.lock().unwrap_or_else(PoisonError::into_inner).open.map(|(stage, _)| stage)
    }
}

struct Long {
    holder: Holder,
    /// `None` for a read transaction.
    kind: Option<HoldKind>,
    ended: Instant,
    ended_unix_ms: u64,
    held: Duration,
    writes: usize,
    longest_write: Duration,
    /// A staged lend's time by stage, in the order first entered.
    stages: Option<Vec<(&'static str, Duration)>>,
    /// A staged lend's rebuilt aggregates and their claims, by stage.
    rebuilt: Vec<(&'static str, Rebuilt)>,
}

#[derive(Default)]
struct Record {
    /// By the writer thread holding it.
    in_progress: HashMap<ThreadId, Current>,
    holders: BTreeMap<Arc<str>, Vec<Slot>>,
    long_holds: VecDeque<Long>,
    long_reads: VecDeque<Long>,
}

fn us(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn ms(duration: Duration) -> f64 {
    us(duration) as f64 / 1000.0
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn push(ring: &mut VecDeque<Long>, long: Long) {
    if ring.len() == LONG {
        ring.pop_front();
    }
    ring.push_back(long);
}

impl Record {
    fn held(&mut self, now: Instant, wall_ms: u64, hold: &mut WriterHold, held: Duration, over: bool) {
        let holder = hold.holder();
        let epoch = now.saturating_duration_since(super::origin()).as_secs() / SLOT_SECONDS;
        let label = if self.holders.len() < HOLDERS || self.holders.contains_key(&holder.0.label) {
            holder.0.label.clone()
        } else {
            "(other)".into()
        };
        let slots = self.holders.entry(label).or_default();
        if slots.is_empty() {
            slots.resize(SLOTS as usize, Slot::default());
        }
        let slot = &mut slots[(epoch % SLOTS) as usize];
        if slot.epoch != epoch || slot.count == 0 {
            *slot = Slot {
                epoch,
                ..Slot::default()
            };
        }
        slot.count += 1;
        slot.over += u64::from(over);
        slot.total_us = slot.total_us.saturating_add(us(held));
        slot.max_us = slot.max_us.max(us(held));
        if over {
            push(&mut self.long_holds, Long {
                holder: holder.clone(),
                kind: Some(hold.kind),
                ended: now,
                ended_unix_ms: wall_ms,
                held,
                writes: hold.writes,
                longest_write: hold.longest.as_ref().map_or(Duration::ZERO, |(_, took)| *took),
                stages: hold.stages.take(),
                rebuilt: std::mem::take(&mut hold.rebuilt),
            });
        }
    }

    fn read(&mut self, now: Instant, wall_ms: u64, holder: Holder, held: Duration) {
        push(&mut self.long_reads, Long {
            holder,
            kind: None,
            ended: now,
            ended_unix_ms: wall_ms,
            held,
            writes: 0,
            longest_write: Duration::ZERO,
            stages: None,
            rebuilt: Vec::new(),
        });
    }

    fn report(&self, now: Instant, wall_ms: u64) -> Value {
        let epoch = now.saturating_duration_since(super::origin()).as_secs() / SLOT_SECONDS;
        let mut holders = self
            .holders
            .iter()
            .map(|(label, slots)| {
                let mut total = Slot::default();
                for slot in slots.iter().filter(|s| s.count > 0 && s.epoch + SLOTS > epoch && s.epoch <= epoch) {
                    total.count += slot.count;
                    total.over += slot.over;
                    total.total_us = total.total_us.saturating_add(slot.total_us);
                    total.max_us = total.max_us.max(slot.max_us);
                }
                (label, total)
            })
            .filter(|(_, total)| total.count > 0)
            .collect::<Vec<_>>();
        holders.sort_by(|a, b| b.1.total_us.cmp(&a.1.total_us).then_with(|| a.0.cmp(b.0)));
        let holders = holders
            .into_iter()
            .take(REPORTED)
            .map(|(label, total)| {
                json!({
                    "holder": &**label,
                    "count": total.count,
                    "over_target": total.over,
                    "total_ms": total.total_us as f64 / 1000.0,
                    "max_ms": total.max_us as f64 / 1000.0,
                })
            })
            .collect::<Vec<_>>();
        let long = |ring: &VecDeque<Long>| {
            ring.iter()
                .rev()
                .filter(|long| now.saturating_duration_since(long.ended) < HOUR)
                .map(|long| {
                    let mut row = long.holder.json();
                    row["started_unix_ms"] = json!(long.ended_unix_ms.saturating_sub(us(long.held) / 1000));
                    row["held_ms"] = json!(ms(long.held));
                    if let Some(kind) = long.kind {
                        row["kind"] = json!(kind.name());
                        row["writes"] = json!(long.writes);
                        row["longest_write_ms"] = json!(ms(long.longest_write));
                    }
                    if let Some(stages) = &long.stages {
                        let staged = stages.iter().map(|(_, spent)| *spent).sum::<Duration>();
                        row["stages"] = stages.iter().map(|(stage, spent)| ((*stage).to_owned(), json!(ms(*spent)))).collect::<Value>();
                        row["unattributed_ms"] = json!(ms(long.held.saturating_sub(staged)));
                    }
                    if !long.rebuilt.is_empty() {
                        row["rebuilt"] = long
                            .rebuilt
                            .iter()
                            .map(|(stage, rebuilt)| ((*stage).to_owned(), rebuilt.json()))
                            .collect::<Value>();
                    }
                    row
                })
                .collect::<Vec<_>>()
        };
        let mut in_progress = self.in_progress.values().collect::<Vec<_>>();
        in_progress.sort_by_key(|current| current.started);
        let in_progress = in_progress
            .into_iter()
            .map(|current| {
                let held = now.saturating_duration_since(current.started);
                let mut row = current.holder.json();
                row["kind"] = json!(current.kind.name());
                row["started_unix_ms"] = json!(wall_ms.saturating_sub(us(held) / 1000));
                row["held_ms"] = json!(ms(held));
                row["writes"] = json!(current.writes);
                if let Some(stage) = current.stage() {
                    row["stage"] = json!(stage);
                }
                row
            })
            .collect::<Vec<_>>();
        json!({
            "waiting": {
                "writer": WRITER_WAITING.load(Ordering::Relaxed),
                "read": READ_WAITING.load(Ordering::Relaxed),
            },
            "in_progress": in_progress,
            "holders": holders,
            "long_holds": long(&self.long_holds),
            "long_reads": long(&self.long_reads),
        })
    }
}

static RECORD: Mutex<Option<Record>> = Mutex::new(None);

fn with_record<T>(work: impl FnOnce(&mut Record) -> T) -> T {
    work(RECORD.lock().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(Record::default))
}

/// One hold of the writer, from when the writer thread began it to when it drops. It counts in
/// the writer-hold window and is charged to its holder: a lend's borrower, or the write in a
/// batch that ran longest.
pub struct WriterHold {
    kind: HoldKind,
    started: Instant,
    first: Holder,
    longest: Option<(Holder, Duration)>,
    writes: usize,
    /// A staged lend's stages, closed when the hold ends.
    stages: Option<Vec<(&'static str, Duration)>>,
    /// A staged lend's rebuilt aggregates, taken when the hold ends.
    rebuilt: Vec<(&'static str, Rebuilt)>,
}

impl WriterHold {
    pub fn start(kind: HoldKind, holder: &Holder) -> Self {
        let started = Instant::now();
        with_record(|record| {
            record.in_progress.insert(std::thread::current().id(), Current {
                holder: holder.clone(),
                kind,
                started,
                writes: 0,
            });
        });
        Self {
            kind,
            started,
            first: holder.clone(),
            longest: None,
            writes: 0,
            stages: None,
            rebuilt: Vec::new(),
        }
    }

    /// A batched write from `holder` begins in this hold.
    pub fn running(&self, holder: &Holder) {
        let writes = self.writes;
        with_record(|record| {
            if let Some(current) = record.in_progress.get_mut(&std::thread::current().id()) {
                current.holder = holder.clone();
                current.writes = writes;
            }
        });
    }

    /// A batched write from `holder` ran for `took` in this hold.
    pub fn ran(&mut self, holder: &Holder, took: Duration) {
        self.writes += 1;
        if self.longest.as_ref().is_none_or(|(_, longest)| took > *longest) {
            self.longest = Some((holder.clone(), took));
        }
    }

    fn holder(&self) -> &Holder {
        self.longest.as_ref().map_or(&self.first, |(holder, _)| holder)
    }
}

impl Drop for WriterHold {
    fn drop(&mut self) {
        let ended = Instant::now();
        let held = ended.saturating_duration_since(self.started);
        // The borrower has given the connection back, so its open stage ends with the hold.
        if self.kind == HoldKind::Lend
            && let Some(clock) = &self.first.0.stages
        {
            let mut clock = clock.lock().unwrap_or_else(PoisonError::into_inner);
            self.stages = Some(clock.close(ended));
            self.rebuilt = clock.rebuilt.clone();
            #[cfg(any(test, feature = "test-support"))]
            {
                let mut recent = RECENT_STAGED.lock().unwrap_or_else(PoisonError::into_inner);
                if recent.len() == LONG {
                    recent.pop_front();
                }
                recent.push_back((
                    self.stages.clone().unwrap_or_default(),
                    self.rebuilt.clone(),
                ));
            }
        }
        if let Some(observer) = &self.holder().0.observer {
            observer.fetch_max(us(held), Ordering::Relaxed);
        }
        let over = super::record_store(StoreWork::WriterHold, held);
        let now = Instant::now();
        let wall = unix_ms();
        with_record(|record| {
            record.in_progress.remove(&std::thread::current().id());
            record.held(now, wall, self, held, over);
        });
    }
}

/// One read transaction. It counts in the read-transaction window; one over its target is
/// charged to the work this thread does when it ends, which is the work that began it.
pub struct ReadHold {
    started: Instant,
    at: &'static Location<'static>,
}

impl ReadHold {
    #[track_caller]
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            at: Location::caller(),
        }
    }
}

impl Drop for ReadHold {
    fn drop(&mut self) {
        let held = self.started.elapsed();
        if super::record_store(StoreWork::ReadTransaction, held) {
            let holder = Holder::asked_at(self.at);
            let now = Instant::now();
            let wall = unix_ms();
            with_record(|record| record.read(now, wall, holder, held));
        }
    }
}

thread_local! {static OBSERVER: RefCell<Option<Arc<AtomicU64>>> = const {RefCell::new(None)};}

/// Run `work`, and return with its result the longest writer hold charged to work this thread
/// asked for during it. Unlike the report, it does not depend on how many other holders the
/// process has, so a test can bound one path while others run beside it.
pub fn longest_during<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    struct Restore(Option<Arc<AtomicU64>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            OBSERVER.with(|observer| *observer.borrow_mut() = previous);
        }
    }
    let observed = Arc::new(AtomicU64::new(0));
    let _restore = Restore(OBSERVER.with(|observer| observer.borrow_mut().replace(observed.clone())));
    let result = work();
    (result, Duration::from_micros(observed.load(Ordering::Relaxed)))
}

/// Stages a lend keeps; a later stage adds to `other`, the last.
const STAGES_KEPT: usize = 16;

/// The aggregates one stage of a staged lend rebuilt, and the claims they replayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rebuilt {
    pub aggregates: u64,
    pub claims: u64,
}

impl Rebuilt {
    fn json(&self) -> Value {
        json!({"aggregates": self.aggregates, "claims": self.claims})
    }
}

/// Where one staged lend's borrower spent its hold so far.
#[derive(Debug, Default)]
struct StageClock {
    open: Option<(&'static str, Instant)>,
    spent: Vec<(&'static str, Duration)>,
    rebuilt: Vec<(&'static str, Rebuilt)>,
    closed: bool,
}

impl StageClock {
    fn add(&mut self, stage: &'static str, spent: Duration) {
        if let Some(entry) = self.spent.iter_mut().find(|(kept, _)| *kept == stage) {
            entry.1 += spent;
        } else if self.spent.len() < STAGES_KEPT - 1 {
            self.spent.push((stage, spent));
        } else if let Some(entry) = self.spent.iter_mut().find(|(kept, _)| *kept == "other") {
            entry.1 += spent;
        } else {
            self.spent.push(("other", spent));
        }
    }

    /// Count one aggregate rebuilt with `claims` claims against the open stage, or `other`.
    fn rebuilt(&mut self, claims: u64) {
        let open = self.open.map_or("other", |(stage, _)| stage);
        let stage = if self.rebuilt.len() < STAGES_KEPT - 1
            || self.rebuilt.iter().any(|(kept, _)| *kept == open)
        {
            open
        } else {
            "other"
        };
        let index = match self.rebuilt.iter().position(|(kept, _)| *kept == stage) {
            Some(index) => index,
            None => {
                self.rebuilt.push((stage, Rebuilt::default()));
                self.rebuilt.len() - 1
            }
        };
        let entry = &mut self.rebuilt[index].1;
        entry.aggregates += 1;
        entry.claims = entry.claims.saturating_add(claims);
    }

    /// End the open stage at `at` and stop. A stage entered after this is not counted.
    fn close(&mut self, at: Instant) -> Vec<(&'static str, Duration)> {
        if let Some((stage, since)) = self.open.take() {
            self.add(stage, at.saturating_duration_since(since));
        }
        self.closed = true;
        self.spent.clone()
    }
}

thread_local! {static STAGES: RefCell<Option<Arc<Mutex<StageClock>>>> = const {RefCell::new(None)};}

#[cfg(test)]
thread_local! {static UNSTAGED: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}

/// Run `work` with every `Stages` scope it begins recording nothing, so a test can compare a
/// staged path with the same path unstaged.
#[cfg(test)]
pub fn unstaged<T>(work: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            UNSTAGED.set(false);
        }
    }
    UNSTAGED.set(true);
    let _restore = Restore;
    work()
}

/// A scope whose writer lend records where its hold went. Begin it before asking for the writer,
/// enter each stage as the borrower reaches it, and end it once the writer is given back: a lend
/// asked for in the scope carries its clock, and the writer thread closes the stage still open
/// when the hold ends. Holds asked for outside it, and batches, are not staged.
pub struct Stages {
    clock: Arc<Mutex<StageClock>>,
    previous: Option<Arc<Mutex<StageClock>>>,
}

impl Stages {
    pub fn begin() -> Self {
        let clock = Arc::new(Mutex::new(StageClock::default()));
        #[cfg(test)]
        if UNSTAGED.get() {
            clock.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
            let previous = STAGES.with(|stages| stages.borrow().clone());
            return Self { clock, previous };
        }
        let previous = STAGES.with(|stages| stages.borrow_mut().replace(clock.clone()));
        Self { clock, previous }
    }

    /// End the open stage and begin `stage`, returning the stage it ended, so a callback can be
    /// charged to its own stage and the caller's resumed after it.
    pub fn enter(&self, stage: &'static str) -> Option<&'static str> {
        enter(&self.clock, stage)
    }

    /// Run `work` as `stage`, then resume the stage it interrupted.
    pub fn during<T>(&self, stage: &'static str, work: impl FnOnce() -> T) -> T {
        during(&self.clock, stage, work)
    }
}

fn enter(clock: &Mutex<StageClock>, stage: &'static str) -> Option<&'static str> {
    let now = Instant::now();
    let mut clock = clock.lock().unwrap_or_else(PoisonError::into_inner);
    if clock.closed {
        return None;
    }
    let ended = clock.open.replace((stage, now));
    if let Some((ended, since)) = ended {
        clock.add(ended, now.saturating_duration_since(since));
    }
    ended.map(|(stage, _)| stage)
}

fn during<T>(clock: &Mutex<StageClock>, stage: &'static str, work: impl FnOnce() -> T) -> T {
    let resumed = enter(clock, stage);
    let result = work();
    if let Some(resumed) = resumed {
        enter(clock, resumed);
    }
    result
}

/// Run `work` as `stage` of the staged lend this thread is in, then resume the stage it
/// interrupted, whose time excludes `work`'s. Outside a `Stages` scope it only runs `work`, so
/// code that both a staged lend and other callers reach can mark its parts. If `work` panics, the
/// stage stays open until the writer closes the lend at the end of its hold, which charges it the
/// rest, as for any stage a borrower leaves open.
pub fn stage<T>(stage: &'static str, work: impl FnOnce() -> T) -> T {
    match STAGES.with(|stages| stages.borrow().clone()) {
        Some(clock) => during(&clock, stage, work),
        None => work(),
    }
}

/// Count one rebuilt aggregate and the claims it replayed against the staged lend's open stage.
/// Outside a `Stages` scope, or once the writer has closed the lend, it does nothing.
pub fn rebuilt(claims: usize) {
    if let Some(clock) = STAGES.with(|stages| stages.borrow().clone()) {
        let mut clock = clock.lock().unwrap_or_else(PoisonError::into_inner);
        if !clock.closed {
            clock.rebuilt(claims as u64);
        }
    }
}

/// A staged lend's stages and rebuilt aggregates, as the writer closed them.
#[cfg(any(test, feature = "test-support"))]
pub type StagedLend = (Vec<(&'static str, Duration)>, Vec<(&'static str, Rebuilt)>);

#[cfg(any(test, feature = "test-support"))]
static RECENT_STAGED: Mutex<VecDeque<StagedLend>> = Mutex::new(VecDeque::new());

/// The last staged lends this process closed, oldest first, whether or not they were long.
#[cfg(any(test, feature = "test-support"))]
pub fn recent_staged() -> Vec<StagedLend> {
    RECENT_STAGED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

impl Drop for Stages {
    fn drop(&mut self) {
        let previous = self.previous.take();
        STAGES.with(|stages| *stages.borrow_mut() = previous);
    }
}

static WRITER_WAITING: AtomicUsize = AtomicUsize::new(0);
static READ_WAITING: AtomicUsize = AtomicUsize::new(0);

/// One caller waiting for the writer or for read admission, until it drops.
pub struct Waiting(&'static AtomicUsize);

impl Waiting {
    /// A caller asked for the writer and has not begun its write.
    pub fn writer() -> Self {
        WRITER_WAITING.fetch_add(1, Ordering::Relaxed);
        Self(&WRITER_WAITING)
    }

    /// A read found no admission free and waits for one.
    pub fn read() -> Self {
        READ_WAITING.fetch_add(1, Ordering::Relaxed);
        Self(&READ_WAITING)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Each writer's hold in progress, oldest first; each holder's writer holds over the last hour;
/// and the last writer holds and read transactions over their target in that hour, newest first.
pub fn report() -> Value {
    let now = Instant::now();
    let wall = unix_ms();
    with_record(|record| record.report(now, wall))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(label: &str) -> Holder {
        Holder(Arc::new(Asked {
            label: label.into(),
            client: Some("member-b".into()),
            at: Location::caller(),
            observer: None,
            stages: None,
        }))
    }

    fn hold(kind: HoldKind, first: &str) -> WriterHold {
        // Built directly so the test does not touch the process-wide record.
        WriterHold {
            kind,
            started: Instant::now(),
            first: holder(first),
            longest: None,
            writes: 0,
            stages: None,
            rebuilt: Vec::new(),
        }
    }

    #[test]
    fn a_batch_is_charged_to_its_longest_write_and_a_lend_to_its_borrower() {
        let mut record = Record::default();
        let now = super::super::origin() + Duration::from_secs(7_200);
        let mut batch = hold(HoldKind::Batch, "/v1/messages");
        batch.ran(&holder("/v1/messages"), Duration::from_millis(2));
        batch.ran(&holder("/v1/internal/replication/receive"), Duration::from_millis(180));
        batch.ran(&holder("/v1/harness-events"), Duration::from_millis(1));
        record.held(now, 1_000_000, &mut batch, Duration::from_millis(190), true);
        std::mem::forget(batch);
        let mut lend = hold(HoldKind::Lend, "retention/trim");
        record.held(now, 1_000_000, &mut lend, Duration::from_millis(30), false);
        record.held(now, 1_000_000, &mut lend, Duration::from_millis(40), false);
        std::mem::forget(lend);
        let report = record.report(now, 1_000_000);
        let holders = report["holders"].as_array().unwrap();
        assert_eq!(holders[0]["holder"], "/v1/internal/replication/receive");
        assert_eq!(holders[0]["over_target"], 1);
        assert_eq!(holders[0]["total_ms"], 190.0);
        assert_eq!(holders[1]["holder"], "retention/trim");
        assert_eq!(holders[1]["count"], 2);
        assert_eq!(holders[1]["max_ms"], 40.0);
        let long = &report["long_holds"].as_array().unwrap();
        assert_eq!(long.len(), 1);
        assert_eq!(long[0]["kind"], "batch");
        assert_eq!(long[0]["writes"], 3);
        assert_eq!(long[0]["longest_write_ms"], 180.0);
        assert_eq!(long[0]["held_ms"], 190.0);
        assert_eq!(long[0]["started_unix_ms"], 1_000_000 - 190);
        assert_eq!(long[0]["client"], "member-b");
        assert!(long[0]["at"].as_str().unwrap().contains("holds.rs"));
    }

    #[test]
    fn a_scope_sees_the_longest_hold_its_thread_asked_for_and_nothing_after_it() {
        let ((), longest) = longest_during(|| {
            for held in [5, 25, 10] {
                let hold = WriterHold::start(HoldKind::Lend, &Holder::current());
                std::thread::sleep(Duration::from_millis(held));
                drop(hold);
            }
        });
        assert!(longest >= Duration::from_millis(25), "{longest:?}");
        assert!(Holder::current().0.observer.is_none());
        let ((), unasked) = longest_during(|| {});
        assert_eq!(unasked, Duration::ZERO);
    }

    #[test]
    fn holds_age_out_of_the_hour_and_the_record_stays_bounded() {
        let mut record = Record::default();
        let now = super::super::origin() + Duration::from_secs(7_200);
        for i in 0..200 {
            let mut hold = hold(HoldKind::Lend, &format!("task-{i}"));
            record.held(now, 5_000_000, &mut hold, Duration::from_millis(150), true);
            std::mem::forget(hold);
        }
        for _ in 0..100 {
            record.read(now, 5_000_000, holder("/v1/internal/replication/export"), Duration::from_secs(3));
        }
        assert_eq!(record.holders.len(), HOLDERS + 1);
        assert!(record.holders.contains_key("(other)"));
        let report = record.report(now, 5_000_000);
        assert_eq!(report["holders"].as_array().unwrap().len(), REPORTED);
        assert_eq!(report["holders"][0]["holder"], "(other)");
        assert_eq!(report["long_holds"].as_array().unwrap().len(), LONG);
        assert_eq!(report["long_holds"][0]["holder"], "task-199");
        let reads = report["long_reads"].as_array().unwrap();
        assert_eq!((reads.len(), reads[0]["held_ms"].clone()), (LONG, json!(3000.0)));
        assert!(reads[0].get("kind").is_none());
        let later = record.report(now + HOUR, 5_000_000 + 3_600_000);
        assert!(later["holders"].as_array().unwrap().is_empty());
        assert!(later["long_holds"].as_array().unwrap().is_empty());
        assert!(later["long_reads"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_writer_charges_each_batch_and_lend_to_the_work_that_asked() {
        let writer = crate::sqlite::WriterConnection::new(
            rusqlite::Connection::open_in_memory().unwrap(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        // Other tests' writers share the process-wide record, so look for these holders only.
        let holding = |label: &str| {
            report()["in_progress"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["holder"] == label)
                .cloned()
        };
        let in_batch = crate::performance::with_cpu(Some("/v1/test/hold-batch"), Some("member-c"), || {
            writer
                .batched(|tx| {
                    tx.execute_batch("CREATE TABLE held(value INTEGER)")?;
                    Ok::<_, rusqlite::Error>(holding("/v1/test/hold-batch"))
                })
                .unwrap()
                .unwrap()
        })
        .expect("the batch names the request that asked while it runs");
        assert_eq!((in_batch["kind"].clone(), in_batch["client"].clone()), (json!("batch"), json!("member-c")));
        assert!(in_batch["at"].as_str().unwrap().contains("holds.rs"), "{in_batch}");
        let in_lend = crate::performance::with_cpu(Some("test/hold-lend"), None, || {
            let guard = writer.write();
            guard.execute_batch("INSERT INTO held VALUES (1)").unwrap();
            holding("test/hold-lend")
        })
        .expect("the lend names the task that borrowed the writer");
        assert_eq!(in_lend["kind"], "lend");
        assert!(in_lend["client"].is_null());
        // The writer ends the lend once the guard gives the connection back.
        writer.batched(|_| Ok::<_, rusqlite::Error>(())).unwrap().unwrap();
        assert!(holding("test/hold-lend").is_none());
    }

    #[test]
    fn a_caller_waiting_for_the_writer_counts_until_its_write_begins() {
        let writer = crate::sqlite::WriterConnection::new(
            rusqlite::Connection::open_in_memory().unwrap(),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        let guard = writer.write();
        let (asked, asking) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                asked.send(()).unwrap();
                writer.batched(|_| Ok::<_, rusqlite::Error>(WRITER_WAITING.load(Ordering::Relaxed))).unwrap().unwrap()
            });
            asking.recv().unwrap();
            // Other tests' writers share the counter, so wait until it counts this caller.
            while WRITER_WAITING.load(Ordering::Relaxed) == 0 {
                std::thread::yield_now();
            }
            assert!(report()["waiting"]["writer"].as_u64().unwrap() >= 1);
            drop(guard);
            waiter.join().unwrap();
        });
    }

    #[test]
    fn the_hold_in_progress_names_its_holder_and_age() {
        let mut record = Record::default();
        let now = super::super::origin() + Duration::from_secs(7_200);
        let other = std::thread::spawn(|| std::thread::current().id()).join().unwrap();
        record.in_progress.insert(other, Current {
            holder: holder("/v1/messages"),
            kind: HoldKind::Batch,
            started: now + Duration::from_millis(101_000),
            writes: 3,
        });
        record.in_progress.insert(std::thread::current().id(), Current {
            holder: holder("/v1/internal/replication/export"),
            kind: HoldKind::Lend,
            started: now,
            writes: 0,
        });
        let report = record.report(now + Duration::from_millis(101_800), 9_000_000);
        let oldest = &report["in_progress"][0];
        assert_eq!(oldest["holder"], "/v1/internal/replication/export");
        assert_eq!(oldest["kind"], "lend");
        assert_eq!(oldest["held_ms"], 101_800.0);
        assert_eq!(oldest["started_unix_ms"], 9_000_000 - 101_800);
        assert_eq!(report["in_progress"][1]["writes"], 3);
    }

    #[test]
    fn a_staged_lend_keeps_a_bounded_set_of_stages_and_stops_at_its_end() {
        const NAMES: [&str; 20] = [
            "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "s12", "s13",
            "s14", "s15", "s16", "s17", "s18", "s19",
        ];
        let mut clock = StageClock::default();
        for name in NAMES {
            clock.add(name, Duration::from_millis(1));
        }
        assert_eq!(clock.spent.len(), STAGES_KEPT);
        let folded = (NAMES.len() - (STAGES_KEPT - 1)) as u64;
        assert_eq!(
            clock.spent[STAGES_KEPT - 1],
            ("other", Duration::from_millis(folded))
        );
        let total = clock.spent.iter().map(|(_, spent)| *spent).sum::<Duration>();
        assert_eq!(total, Duration::from_millis(20));
        // A stage entered after the writer closed the clock is not counted.
        let stages = Stages::begin();
        stages.enter("begin");
        let closed = stages.clock.lock().unwrap().close(Instant::now());
        assert_eq!(closed.iter().map(|(stage, _)| *stage).collect::<Vec<_>>(), ["begin"]);
        assert_eq!(stages.enter("late"), None);
        assert_eq!(stages.clock.lock().unwrap().spent.len(), 1);
    }

    #[test]
    fn a_lend_asked_for_in_a_stage_scope_reports_its_stages_and_the_rest_unattributed() {
        let mut record = Record::default();
        let now = super::super::origin() + Duration::from_secs(7_200);
        let stages = Stages::begin();
        let asked = Holder::current();
        drop(stages);
        assert!(Holder::current().0.stages.is_none(), "the scope ends with its guard");
        let clock = asked.0.stages.clone().unwrap();
        clock.lock().unwrap().spent = vec![("incremental", Duration::from_millis(150)), ("commit", Duration::from_millis(30))];
        let mut lend = WriterHold {
            kind: HoldKind::Lend,
            started: Instant::now(),
            first: asked,
            longest: None,
            writes: 0,
            stages: Some(clock.lock().unwrap().spent.clone()),
            rebuilt: vec![(
                "rebuild-tree",
                Rebuilt {
                    aggregates: 2,
                    claims: 40,
                },
            )],
        };
        record.held(now, 1_000_000, &mut lend, Duration::from_millis(200), true);
        std::mem::forget(lend);
        let long = &record.report(now, 1_000_000)["long_holds"][0];
        assert_eq!(long["stages"], json!({"incremental": 150.0, "commit": 30.0}));
        assert_eq!(long["unattributed_ms"], 20.0);
        assert_eq!(
            long["rebuilt"],
            json!({"rebuild-tree": {"aggregates": 2, "claims": 40}})
        );
        // A batch inside a scope is charged as before, with no stages.
        let mut batch = hold(HoldKind::Batch, "/v1/messages");
        record.held(now, 1_000_000, &mut batch, Duration::from_millis(200), true);
        std::mem::forget(batch);
        let batch = &record.report(now, 1_000_000)["long_holds"][0];
        assert!(batch.get("stages").is_none() && batch.get("unattributed_ms").is_none(), "{batch}");
    }

    #[test]
    fn a_stage_entered_by_called_code_pauses_the_stage_it_interrupts_and_counts_rebuilds() {
        // Outside a scope, a stage only runs its work and a rebuild counts nothing.
        assert_eq!(stage("rebuild-tree", || 7), 7);
        rebuilt(5);
        let stages = Stages::begin();
        stages.enter("incremental");
        stage("rebuild-tree", || {
            rebuilt(40);
            rebuilt(2);
            std::thread::sleep(Duration::from_millis(300));
        });
        rebuilt(3);
        let mut clock = stages.clock.lock().unwrap();
        let spent = clock.close(Instant::now());
        let of = |name: &str| spent.iter().find(|(stage, _)| *stage == name).unwrap().1;
        assert!(of("rebuild-tree") >= Duration::from_millis(300));
        assert!(
            of("incremental") < Duration::from_millis(100),
            "the stage a rebuild interrupts does not also count its time: {spent:?}"
        );
        assert_eq!(
            clock.rebuilt,
            [
                (
                    "rebuild-tree",
                    Rebuilt {
                        aggregates: 2,
                        claims: 42
                    }
                ),
                (
                    "incremental",
                    Rebuilt {
                        aggregates: 1,
                        claims: 3
                    }
                ),
            ]
        );
        drop(clock);
        // Once the writer has closed the lend, a late rebuild is not counted.
        rebuilt(9);
        assert_eq!(stages.clock.lock().unwrap().rebuilt.len(), 2);
    }
}
