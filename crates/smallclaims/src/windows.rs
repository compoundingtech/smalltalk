//! Rolling 1-minute, 5-minute and 1-hour latency windows, in memory and bounded. No graph claims,
//! no disk writes and no sampler threads: a sample is added where the work finishes, and a window
//! ages out when it is next written or read.
//!
//! Each window is a ring of slots (12 of 5 s, 10 of 30 s and 12 of 5 min), and each slot a sparse
//! log histogram with 16 buckets per doubling, so a percentile is within 1/16 of the sample it
//! names. A percentile reports its bucket's upper bound, never above the window's exact maximum.
//! A slot keeps only the buckets it saw, so a series costs at most 34 slots of 976 buckets and an
//! idle series costs nothing past its slots. The share over target is counted exactly when the
//! sample is added, against the target the caller gives it.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The windows every series keeps: name, slot width and slot count.
const TIERS: [(&str, u64, usize); 3] = [("1m", 5, 12), ("5m", 30, 10), ("1h", 300, 12)];

/// Sixteen buckets for each power of two; values under 16 µs are exact.
const SUB: u64 = 16;

fn origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

fn bucket(us: u64) -> u16 {
    if us < SUB {
        return us as u16;
    }
    let exponent = 63 - u64::from(us.leading_zeros());
    let sub = (us >> (exponent - 4)) - SUB;
    (SUB + (exponent - 4) * SUB + sub) as u16
}

/// The largest value in `bucket`, in microseconds.
fn upper(bucket: u16) -> u64 {
    let bucket = u64::from(bucket);
    if bucket < SUB {
        return bucket;
    }
    let exponent = (bucket - SUB) / SUB + 4;
    let sub = (bucket - SUB) % SUB;
    let bound = u128::from(SUB + sub + 1) << (exponent - 4);
    u64::try_from(bound - 1).unwrap_or(u64::MAX)
}

#[derive(Clone, Default)]
struct Histogram {
    count: u64,
    over: u64,
    max_us: u64,
    /// Sorted by bucket.
    buckets: Vec<(u16, u32)>,
}

impl Histogram {
    fn add(&mut self, us: u64, over: bool) {
        self.count += 1;
        self.over += u64::from(over);
        self.max_us = self.max_us.max(us);
        let bucket = bucket(us);
        match self.buckets.binary_search_by_key(&bucket, |(b, _)| *b) {
            Ok(at) => self.buckets[at].1 = self.buckets[at].1.saturating_add(1),
            Err(at) => self.buckets.insert(at, (bucket, 1)),
        }
    }

    fn merge(&mut self, other: &Histogram) {
        self.count += other.count;
        self.over += other.over;
        self.max_us = self.max_us.max(other.max_us);
        for &(bucket, count) in &other.buckets {
            match self.buckets.binary_search_by_key(&bucket, |(b, _)| *b) {
                Ok(at) => self.buckets[at].1 = self.buckets[at].1.saturating_add(count),
                Err(at) => self.buckets.insert(at, (bucket, count)),
            }
        }
    }

    /// The `percent`th percentile's bucket bound, in microseconds.
    fn percentile(&self, percent: u64) -> u64 {
        let rank = (self.count * percent).div_ceil(100).max(1);
        let mut seen = 0;
        for &(bucket, count) in &self.buckets {
            seen += u64::from(count);
            if seen >= rank {
                return upper(bucket).min(self.max_us);
            }
        }
        self.max_us
    }
}

#[derive(Clone, Default)]
struct Slot {
    epoch: u64,
    histogram: Histogram,
}

/// One series: what one route, statement class or hold took, in each window.
#[derive(Clone, Default)]
pub struct Series {
    rings: [Vec<Slot>; 3],
}

fn ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

impl Series {
    /// Add one sample that finished at `now`; `over` says whether it missed its target.
    pub fn record(&mut self, now: Instant, duration: Duration, over: bool) {
        let us = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let since = now.saturating_duration_since(origin()).as_secs();
        for (ring, (_, width, slots)) in self.rings.iter_mut().zip(TIERS) {
            let epoch = since / width;
            if ring.is_empty() {
                ring.resize(slots, Slot::default());
            }
            let slot = &mut ring[(epoch % slots as u64) as usize];
            if slot.epoch != epoch || slot.histogram.count == 0 {
                *slot = Slot {
                    epoch,
                    histogram: Histogram::default(),
                };
            }
            slot.histogram.add(us, over);
        }
    }

    fn window(&self, tier: usize, now: Instant) -> Histogram {
        let (_, width, slots) = TIERS[tier];
        let epoch = now.saturating_duration_since(origin()).as_secs() / width;
        let mut merged = Histogram::default();
        for slot in &self.rings[tier] {
            if slot.histogram.count > 0 && slot.epoch + slots as u64 > epoch && slot.epoch <= epoch
            {
                merged.merge(&slot.histogram);
            }
        }
        merged
    }

    /// Merge `other`'s samples into this series, for a total over several series.
    pub fn absorb(&mut self, other: &Series) {
        for (ring, theirs) in self.rings.iter_mut().zip(&other.rings) {
            if ring.is_empty() {
                ring.clone_from(theirs);
                continue;
            }
            for (mine, theirs) in ring.iter_mut().zip(theirs) {
                if theirs.histogram.count == 0 {
                    continue;
                }
                if mine.histogram.count == 0 || mine.epoch < theirs.epoch {
                    mine.clone_from(theirs);
                } else if mine.epoch == theirs.epoch {
                    mine.histogram.merge(&theirs.histogram);
                }
            }
        }
    }

    /// Whether any window still holds a sample.
    pub fn is_empty(&self, now: Instant) -> bool {
        (0..TIERS.len()).all(|tier| self.window(tier, now).count == 0)
    }

    /// Each window's count, p50, p99, max and share over target, in milliseconds.
    pub fn snapshot(&self, now: Instant) -> Value {
        let mut windows = serde_json::Map::new();
        for (tier, (name, _, _)) in TIERS.iter().enumerate() {
            let window = self.window(tier, now);
            windows.insert(
                (*name).into(),
                json!({
                    "count": window.count,
                    "p50_ms": ms(window.percentile(50)),
                    "p99_ms": ms(window.percentile(99)),
                    "max_ms": ms(window.max_us),
                    "over_target": window.over,
                    "over_target_share": if window.count == 0 {
                        0.0
                    } else {
                        window.over as f64 / window.count as f64
                    },
                }),
            );
        }
        Value::Object(windows)
    }
}

/// What the store's own work is counted under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreWork {
    /// One SQLite statement, from its first step to its reset.
    Statement,
    /// A read snapshot, from BEGIN to its end.
    ReadTransaction,
    /// A write transaction, from BEGIN to COMMIT or rollback.
    WriteTransaction,
    /// The single writer held by one batch or one lent write.
    WriterHold,
}

impl StoreWork {
    pub const ALL: [StoreWork; 4] = [
        StoreWork::Statement,
        StoreWork::ReadTransaction,
        StoreWork::WriteTransaction,
        StoreWork::WriterHold,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StoreWork::Statement => "sql-statement",
            StoreWork::ReadTransaction => "read-transaction",
            StoreWork::WriteTransaction => "write-transaction",
            StoreWork::WriterHold => "writer-hold",
        }
    }
}

struct StoreSeries {
    /// A sample longer than this missed its target; `None` before a target is set.
    target: Option<Duration>,
    series: Series,
}

static STORE: [Mutex<StoreSeries>; 4] = [const {
    Mutex::new(StoreSeries {
        target: None,
        series: Series {
            rings: [Vec::new(), Vec::new(), Vec::new()],
        },
    })
}; 4];

fn store(kind: StoreWork) -> &'static Mutex<StoreSeries> {
    &STORE[StoreWork::ALL.iter().position(|k| *k == kind).unwrap_or(0)]
}

/// The target a store sample is counted over, from the daemon's targets file.
pub fn set_store_target(kind: StoreWork, target: Option<Duration>) {
    store(kind)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .target = target;
}

pub fn record_store(kind: StoreWork, duration: Duration) {
    let now = Instant::now();
    let mut store = store(kind).lock().unwrap_or_else(PoisonError::into_inner);
    let over = store.target.is_some_and(|target| duration > target);
    store.series.record(now, duration, over);
}

/// A copy of one store series, to read or to merge with another.
pub fn store_series(kind: StoreWork) -> Series {
    store(kind)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .series
        .clone()
}

/// Records how long it lives in `kind` when dropped.
pub struct Timer {
    kind: StoreWork,
    started: Instant,
}

impl Timer {
    pub fn start(kind: StoreWork) -> Self {
        Self {
            kind,
            started: Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        record_store(self.kind, self.started.elapsed());
    }
}

/// Process CPU over each window, from a CPU reading taken when each slot opens.
#[derive(Default)]
pub struct Cpu {
    rings: [Vec<Option<(u64, Instant, u64)>>; 3],
}

impl Cpu {
    /// Note the process's CPU at the start of each slot. Reads the clock only when a slot opens.
    pub fn note(&mut self, now: Instant) {
        let since = now.saturating_duration_since(origin()).as_secs();
        let mut reading = None;
        for (ring, (_, width, slots)) in self.rings.iter_mut().zip(TIERS) {
            let epoch = since / width;
            if ring.is_empty() {
                ring.resize(slots, None);
            }
            let slot = &mut ring[(epoch % slots as u64) as usize];
            if slot.is_none_or(|(at, _, _)| at != epoch) {
                let cpu = *reading.get_or_insert_with(process_cpu_ns);
                *slot = Some((epoch, now, cpu));
            }
        }
    }

    /// Cores used over each window, measured from its oldest reading to now.
    pub fn snapshot(&mut self, now: Instant) -> Value {
        self.note(now);
        let cpu_now = process_cpu_ns();
        let since = now.saturating_duration_since(origin()).as_secs();
        let mut windows = serde_json::Map::new();
        for (ring, (name, width, slots)) in self.rings.iter().zip(TIERS) {
            let epoch = since / width;
            let oldest = ring
                .iter()
                .flatten()
                .filter(|(at, _, _)| at + slots as u64 > epoch && *at <= epoch)
                .min_by_key(|(_, at, _)| *at);
            let (cores, seconds) = oldest
                .map(|(_, at, cpu)| {
                    let wall = now.saturating_duration_since(*at).as_secs_f64();
                    let used = cpu_now.saturating_sub(*cpu) as f64 / 1e9;
                    (if wall > 0.0 { used / wall } else { 0.0 }, wall)
                })
                .unwrap_or((0.0, 0.0));
            windows.insert(
                name.into(),
                json!({"cores": (cores * 1000.0).round() / 1000.0, "measured_seconds": seconds.round()}),
            );
        }
        Value::Object(windows)
    }
}

fn process_cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to a valid timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) } == 0 {
        (time.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(time.tv_nsec as u64)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> Instant {
        origin() + Duration::from_secs(seconds)
    }

    #[test]
    fn buckets_are_contiguous_and_within_a_sixteenth() {
        let mut previous = 0;
        for us in 0..200_000_u64 {
            let b = bucket(us);
            assert!(b == previous || b == previous + 1, "{us}");
            assert!(upper(b) >= us);
            assert!(upper(b) - us <= us / 16, "{us} -> {}", upper(b));
            previous = b;
        }
        assert!(bucket(u64::MAX) < 976);
        assert_eq!(upper(bucket(u64::MAX)), u64::MAX);
    }

    #[test]
    fn windows_report_percentiles_and_the_exact_share_over_target() {
        let mut series = Series::default();
        for ms in 1..=100 {
            series.record(at(3_600), Duration::from_millis(ms), ms > 90);
        }
        let report = series.snapshot(at(3_600));
        for window in ["1m", "5m", "1h"] {
            let window = &report[window];
            assert_eq!(window["count"], 100);
            assert_eq!(window["over_target"], 10);
            assert_eq!(window["over_target_share"], 0.1);
            assert_eq!(window["max_ms"], 100.0);
            let p50 = window["p50_ms"].as_f64().unwrap();
            assert!((50.0..=50.0 * 17.0 / 16.0).contains(&p50), "{p50}");
            let p99 = window["p99_ms"].as_f64().unwrap();
            assert!((99.0..=100.0).contains(&p99), "{p99}");
        }
    }

    #[test]
    fn each_window_ages_out_on_its_own() {
        let mut series = Series::default();
        series.record(at(7_200), Duration::from_millis(5), false);
        let count = |series: &Series, seconds, window: &str| {
            series.snapshot(at(seconds))[window]["count"].clone()
        };
        assert_eq!(count(&series, 7_200 + 59, "1m"), 1);
        assert_eq!(count(&series, 7_200 + 60, "1m"), 0);
        assert_eq!(count(&series, 7_200 + 299, "5m"), 1);
        assert_eq!(count(&series, 7_200 + 300, "5m"), 0);
        assert_eq!(count(&series, 7_200 + 3_599, "1h"), 1);
        assert_eq!(count(&series, 7_200 + 3_600, "1h"), 0);
        assert!(series.is_empty(at(7_200 + 3_600)));
        // A slot reused a window later starts empty.
        series.record(at(7_200 + 3_600), Duration::from_millis(7), false);
        assert_eq!(count(&series, 7_200 + 3_600, "1h"), 1);
        assert_eq!(series.snapshot(at(7_200 + 3_600))["1h"]["max_ms"], 7.0);
    }

    #[test]
    fn a_busy_series_stays_bounded() {
        let mut series = Series::default();
        for i in 0..1_000_000_u64 {
            series.record(at(10_800 + i / 300), Duration::from_micros(i * 7919 % 10_000_000), false);
        }
        for ring in &series.rings {
            assert!(ring.len() <= 12);
            assert!(ring.iter().all(|slot| slot.histogram.buckets.len() < 976));
        }
    }

    #[test]
    fn absorbing_series_totals_their_samples() {
        let mut one = Series::default();
        let mut two = Series::default();
        one.record(at(14_400), Duration::from_millis(1), false);
        two.record(at(14_400), Duration::from_millis(300), true);
        two.record(at(14_400 - 120), Duration::from_millis(2), false);
        let mut total = Series::default();
        total.absorb(&one);
        total.absorb(&two);
        let report = total.snapshot(at(14_400));
        assert_eq!(report["1m"]["count"], 2);
        assert_eq!(report["5m"]["count"], 3);
        assert_eq!(report["5m"]["over_target"], 1);
        assert_eq!(report["1h"]["max_ms"], 300.0);
    }

    #[test]
    fn cpu_windows_measure_from_their_oldest_reading() {
        let mut cpu = Cpu::default();
        cpu.note(Instant::now());
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(30) {
            std::hint::black_box((0..1000).sum::<u64>());
        }
        let report = cpu.snapshot(Instant::now());
        for window in ["1m", "5m", "1h"] {
            let cores = report[window]["cores"].as_f64().unwrap();
            assert!(cores > 0.0 && cores < 64.0, "{window}: {cores}");
        }
    }
}
