//! Who held the store's single writer, and who kept a read transaction open, and for how long.
//!
//! Always on and bounded, like the windows beside it. A holder is the request route or background
//! task its thread worked for when it asked (`performance::current`), with the client that sent
//! the request, or the thread's name when it worked for neither, and always the code location
//! that asked. The record keeps each writer's hold in progress (a daemon has one writer per
//! store), each holder's writer holds over the last hour in five-minute slots, and the last 64
//! writer holds and 64 read transactions over their target. A read under its target costs one
//! clock read and is not attributed.

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
        Self(Arc::new(Asked {
            label,
            client,
            at,
            observer,
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

struct Long {
    holder: Holder,
    /// `None` for a read transaction.
    kind: Option<HoldKind>,
    ended: Instant,
    ended_unix_ms: u64,
    held: Duration,
    writes: usize,
    longest_write: Duration,
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
    fn held(&mut self, now: Instant, wall_ms: u64, hold: &WriterHold, held: Duration, over: bool) {
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
        let held = self.started.elapsed();
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
        record.held(now, 1_000_000, &batch, Duration::from_millis(190), true);
        std::mem::forget(batch);
        let lend = hold(HoldKind::Lend, "retention/trim");
        record.held(now, 1_000_000, &lend, Duration::from_millis(30), false);
        record.held(now, 1_000_000, &lend, Duration::from_millis(40), false);
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
            let hold = hold(HoldKind::Lend, &format!("task-{i}"));
            record.held(now, 5_000_000, &hold, Duration::from_millis(150), true);
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
}
