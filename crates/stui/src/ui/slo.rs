//! Device-only quality accounting. Recording uses fixed storage; all JSON and disk work
//! runs on the hourly writer, never on the input/presentation path.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const BUCKET_UPPER_MS: [Option<u64>; 17] = [
    Some(1),
    Some(2),
    Some(4),
    Some(8),
    Some(16),
    Some(32),
    Some(64),
    Some(128),
    Some(256),
    Some(512),
    Some(1024),
    Some(2048),
    Some(4096),
    Some(8192),
    Some(16384),
    Some(32768),
    None,
];
pub const COUNTERS: [&str; 18] = [
    "active_ms",
    "panics",
    "silent_exits",
    "launches",
    "first_screen_top_bar_flips",
    "first_screen_placeholder_flips",
    "conversation_opens",
    "open_not_bound",
    "open_remote_unavailable",
    "open_cannot_load",
    "drafts_restored_unclean",
    "not_live_ms",
    "direct_attach_attempts",
    "direct_attach_success",
    "gateway_attach",
    "terminal_open_errors",
    "dead_terminal_tabs",
    "measurement_overflow",
];
pub const HISTOGRAMS: [&str; 11] = [
    "first_frame_ms",
    "first_real_state_ms",
    "conversation_open_ms",
    "key_to_redraw_ms",
    "freshness_ms",
    "reconnect_ms",
    "terminal_open_ms",
    "alert_landed_ms",
    "send_echo_ms",
    "send_ack_ms",
    "send_visible_ms",
];
/// Starting guesses, to revisit after one week. Shared with st3-views/slo.ts.
pub const TARGETS: [(&str, f64); 14] = [
    ("panics_max", 0.0),
    ("silent_exits_per_100_active_hours_lt", 1.0),
    ("first_frame_p95_ms_lt", 300.0),
    ("first_real_state_p95_ms_lt", 2000.0),
    ("conversation_open_p50_ms_lt", 300.0),
    ("conversation_open_p95_ms_lt", 1000.0),
    ("conversation_error_percent_lt", 0.5),
    ("key_to_redraw_p95_ms_lt", 50.0),
    ("reconnect_p95_ms_lt", 10000.0),
    ("direct_attach_success_percent_gte", 99.0),
    ("terminal_open_p95_ms_lt", 1000.0),
    ("alert_landed_p95_ms_lt", 2000.0),
    // A message sent from here: pressing send to the box showing it, to st accepting it, to the
    // conversation holding it as sent (Nathan, 2026-10-13: sending still feels a little slow).
    ("send_echo_p99_ms_lt", 100.0),
    ("send_visible_p99_ms_lt", 200.0),
];

pub struct Counters {
    counts: [AtomicU64; COUNTERS.len()],
    histograms: [[AtomicU64; 17]; HISTOGRAMS.len()],
}
impl Counters {
    const fn new() -> Self {
        Self {
            counts: [const { AtomicU64::new(0) }; COUNTERS.len()],
            histograms: [const { [const { AtomicU64::new(0) }; 17] }; HISTOGRAMS.len()],
        }
    }
    pub fn add(&self, name: &str, n: u64) {
        if let Some(i) = COUNTERS.iter().position(|key| *key == name) {
            self.counts[i].fetch_add(n, Ordering::Relaxed);
        }
    }
    pub fn observe(&self, name: &str, elapsed: Duration) {
        if let Some(i) = HISTOGRAMS.iter().position(|key| *key == name) {
            self.histograms[i][bucket(elapsed)].fetch_add(1, Ordering::Relaxed);
        }
    }
    fn take(&self, window_start: i64, host: &str, build: &str) -> Line {
        Line {
            version: 1,
            window_start,
            host: host.into(),
            build: build.into(),
            bucket_upper_ms: BUCKET_UPPER_MS,
            counters: COUNTERS
                .iter()
                .enumerate()
                .map(|(i, k)| ((*k).into(), self.counts[i].swap(0, Ordering::Relaxed)))
                .collect(),
            histograms: HISTOGRAMS
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    (
                        (*k).into(),
                        std::array::from_fn(|b| self.histograms[i][b].swap(0, Ordering::Relaxed)),
                    )
                })
                .collect(),
            targets: TARGETS.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        }
    }
}
pub static METRICS: Counters = Counters::new();
static ENABLED: AtomicBool = AtomicBool::new(false);
fn bucket(elapsed: Duration) -> usize {
    let ms = elapsed.as_nanos().div_ceil(1_000_000);
    BUCKET_UPPER_MS
        .iter()
        .position(|upper| upper.is_none_or(|upper| ms <= upper as u128))
        .unwrap_or(16)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub version: u8,
    pub window_start: i64,
    pub host: String,
    pub build: String,
    pub bucket_upper_ms: [Option<u64>; 17],
    pub counters: BTreeMap<String, u64>,
    pub histograms: BTreeMap<String, [u64; 17]>,
    pub targets: BTreeMap<String, f64>,
}
impl Line {
    fn merge(&mut self, other: Line) {
        self.build = other.build;
        for (k, v) in other.counters {
            let count = self.counters.entry(k).or_default();
            *count = count.saturating_add(v);
        }
        for (k, v) in other.histograms {
            let counts = self.histograms.entry(k).or_insert([0; 17]);
            for (count, v) in counts.iter_mut().zip(v) {
                *count = count.saturating_add(v);
            }
        }
    }
}
pub fn directory() -> Option<PathBuf> {
    super::drafts_store::path()?.parent().map(|p| p.join("slo"))
}
const HOURS: i64 = 14 * 24;
const MAX_FILE_BYTES: u64 = 256 * 1024;
fn read_lines(file: &Path) -> Vec<Line> {
    if !std::fs::metadata(file).is_ok_and(|m| m.len() <= MAX_FILE_BYTES) {
        return Vec::new();
    }
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .take(24)
        .filter_map(|line| serde_json::from_str::<Line>(line).ok())
        .collect()
}
fn day_of(path: &Path) -> i64 {
    path.file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or(i64::MIN)
}
fn files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect()
}
fn private_open(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
/// Writers from simultaneous stuis add to the same hour under a disk lock. Daily files
/// rotate on the hour boundary; exactly 336 hourly rows survive, including partial hours.
fn write_line(dir: &Path, line: Line) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let lock = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(dir.join("writer.lock"))?
    };
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A shutdown flush can race another process's next-hour write. Never delete
    // that newer hour while adding an older process's final contribution.
    let paths = files(dir);
    let newest = paths.iter().max_by_key(|path| day_of(path));
    let newest_rows = newest.map(|path| read_lines(path)).unwrap_or_default();
    let latest = newest_rows
        .iter()
        .map(|row| row.window_start)
        .max()
        .unwrap_or(line.window_start)
        .max(line.window_start);
    let cutoff = latest - (HOURS - 1) * 3600;
    if line.window_start < cutoff {
        return Ok(());
    }
    let day = line.window_start.div_euclid(86400);
    let file = dir.join(format!("{day}.jsonl"));
    let mut rows = if newest == Some(&file) {
        newest_rows
    } else {
        read_lines(&file)
    };
    rows.retain(|row| row.window_start >= cutoff);
    if let Some(existing) = rows
        .iter_mut()
        .find(|r| r.window_start == line.window_start && r.host == line.host)
    {
        existing.merge(line);
    } else {
        rows.push(line);
    }
    rows.sort_by_key(|r| r.window_start);
    if rows.len() > 24 {
        rows.drain(..rows.len() - 24);
    }
    let partial = dir.join("hour.partial");
    let mut out = private_open(&partial)?;
    for row in &rows {
        serde_json::to_writer(&mut out, row)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    std::fs::rename(&partial, &file)?;
    let cutoff_day = cutoff.div_euclid(86400);
    for old in paths {
        if old == file {
            continue;
        }
        let old_day = day_of(&old);
        if old_day < cutoff_day {
            let _ = std::fs::remove_file(old);
        } else if old_day == cutoff_day {
            // Only the oldest day can have expired rows. Other days need no reads
            // or rewrites when this hour changes.
            let mut rows = read_lines(&old);
            let before = rows.len();
            rows.retain(|row| row.window_start >= cutoff);
            if rows.len() == before {
                continue;
            }
            if rows.is_empty() {
                let _ = std::fs::remove_file(old);
                continue;
            }
            let mut out = private_open(&partial)?;
            for row in rows {
                serde_json::to_writer(&mut out, &row)?;
                out.write_all(b"\n")?;
            }
            out.flush()?;
            std::fs::rename(&partial, old)?;
        }
    }
    Ok(())
}
fn hour() -> i64 {
    // Unix time for the start of the current local wall-clock hour (also supports half-hour zones).
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.timestamp() - i64::from(now.minute() * 60 + now.second())
}
fn host() -> String {
    let mut bytes = [0u8; 256];
    if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return "unknown".into();
    }
    String::from_utf8_lossy(&bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())])
        .into_owned()
}
/// A panic is persisted synchronously by the existing panic hook, so aborting cannot
/// lose it with the in-memory hour. Only fixed counts are written, never the panic text.
pub fn panic() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if std::thread::current().name() == Some("stui-slo") {
        return;
    }
    if let Some(dir) = directory() {
        let counters = Counters::new();
        counters.add("panics", 1);
        let _ = write_line(
            &dir,
            counters.take(hour(), &host(), crate::version::display_version()),
        );
    }
}

pub struct Session {
    stop: mpsc::Sender<()>,
    writer: Option<std::thread::JoinHandle<()>>,
}
impl Session {
    pub fn start() -> Self {
        Self::start_in(directory())
    }
    fn start_in(directory: Option<PathBuf>) -> Self {
        ENABLED.store(true, Ordering::Relaxed);
        METRICS.add("launches", 1);
        let (stop, receiver) = mpsc::channel();
        let writer = directory.and_then(|dir| {
            // Failure disables disk recording for this run. Never create a state directory.
            std::fs::create_dir_all(&dir).ok()?;
            let host = host();
            let build = crate::version::display_version();
            std::thread::Builder::new()
                .name("stui-slo".into())
                .spawn(move || {
                    let mut window = hour();
                    let mut since = Instant::now();
                    loop {
                        let seconds =
                            (window + 3600 - chrono::Local::now().timestamp()).max(1) as u64;
                        let stopped = !matches!(
                            receiver.recv_timeout(Duration::from_secs(seconds)),
                            Err(mpsc::RecvTimeoutError::Timeout)
                        );
                        METRICS.add(
                            "active_ms",
                            since.elapsed().as_millis().min(u64::MAX as u128) as u64,
                        );
                        since = Instant::now();
                        let _ = write_line(&dir, METRICS.take(window, &host, build));
                        if stopped {
                            break;
                        }
                        window = hour();
                    }
                })
                .ok()
        });
        Self { stop, writer }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        ENABLED.store(false, Ordering::Relaxed);
    }
}

/// Fixed pending timestamps: the draw path records no strings, acquires no locks and
/// allocates nothing. Overflows are explicit rather than silently losing measurements.
pub struct Draw {
    metrics: &'static Counters,
    started: Instant,
    first: bool,
    real: bool,
    signature: Option<(u8, u8)>,
    keys: [Option<Instant>; 64],
    frames: [Option<(Instant, u8)>; 128],
    down: Option<Instant>,
    ever_live: bool,
    last_live: Instant,
}
impl Draw {
    pub fn new(started: Instant) -> Self {
        Self::with_metrics(started, &METRICS)
    }
    fn with_metrics(started: Instant, metrics: &'static Counters) -> Self {
        Self {
            metrics,
            started,
            first: false,
            real: false,
            signature: None,
            keys: [None; 64],
            frames: [None; 128],
            down: Some(started),
            ever_live: false,
            last_live: started,
        }
    }
    fn queue(slots: &mut [Option<Instant>], at: Instant, metrics: &Counters) {
        if let Some(slot) = slots.iter_mut().find(|s| s.is_none()) {
            *slot = Some(at);
        } else {
            metrics.add("measurement_overflow", 1);
        }
    }
    pub fn key(&mut self) {
        Self::queue(&mut self.keys, Instant::now(), self.metrics);
    }
    pub fn frame(&mut self, at: Instant, kind: u8) {
        if let Some(slot) = self.frames.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((at, kind));
        } else {
            self.metrics.add("measurement_overflow", 1);
        }
    }
    pub fn live(&mut self, live: bool) {
        let now = Instant::now();
        if self.down.is_some() {
            self.metrics.add(
                "not_live_ms",
                now.duration_since(self.last_live).as_millis() as u64,
            );
        }
        self.last_live = now;
        if live {
            if let Some(at) = self.down.take()
                && self.ever_live
            {
                self.metrics.observe("reconnect_ms", at.elapsed());
            }
            self.ever_live = true;
        } else if self.down.is_none() {
            self.down = Some(now);
        }
    }
    pub fn presented(&mut self, settled: bool, top: u8, placeholders: u8, visible: u8) {
        if !self.first {
            self.metrics
                .observe("first_frame_ms", self.started.elapsed());
            self.first = true;
        }
        if !self.real {
            if let Some((last_top, last_placeholders)) = self.signature {
                if top != last_top {
                    self.metrics.add("first_screen_top_bar_flips", 1);
                }
                if placeholders != last_placeholders {
                    self.metrics.add("first_screen_placeholder_flips", 1);
                }
            }
            self.signature = Some((top, placeholders));
            if settled {
                self.metrics
                    .observe("first_real_state_ms", self.started.elapsed());
                self.real = true;
            }
        }
        for at in &mut self.keys {
            if let Some(at) = at.take() {
                self.metrics.observe("key_to_redraw_ms", at.elapsed());
            }
        }
        for at in &mut self.frames {
            if let Some((at, kind)) = at.take()
                && visible & kind != 0
            {
                self.metrics.observe("freshness_ms", at.elapsed());
            }
        }
    }
}
pub fn print_history() {
    println!("Local stui quality: retained hours (milliseconds; '-' means no samples)");
    let mut rows = directory()
        .map(|dir| {
            files(&dir)
                .iter()
                .flat_map(|p| read_lines(p))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    rows.sort_by_key(|r| r.window_start);
    for row in rows {
        let date = chrono::DateTime::from_timestamp(row.window_start, 0)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M %z")
                    .to_string()
            })
            .unwrap_or_default();
        println!("\n{date}  {}  {}", row.host, row.build);
        println!("  Counter                                  Value");
        for key in COUNTERS {
            println!(
                "  {key:<39} {:>8}",
                row.counters.get(key).copied().unwrap_or(0)
            );
        }
        println!("  Latency                                 Samples         p50         p95");
        for key in HISTOGRAMS {
            let counts = row.histograms.get(key);
            println!(
                "  {key:<39} {:>7} {:>11} {:>11}",
                counts
                    .map(|counts| counts.iter().fold(0u64, |sum, n| sum.saturating_add(*n)))
                    .unwrap_or(0),
                percentile(counts, 50),
                percentile(counts, 95)
            );
        }
        // Print the historical targets rather than silently judging an old hour with
        // targets changed in a newer binary. Percentiles are bucket upper bounds.
        println!("  Targets (initial guesses):");
        for (key, value) in &row.targets {
            println!("    {key}: {value}");
        }
    }
}
fn percentile(counts: Option<&[u64; 17]>, q: u64) -> String {
    let Some(counts) = counts else {
        return "-".into();
    };
    let total = counts.iter().fold(0u64, |sum, n| sum.saturating_add(*n));
    if total == 0 {
        return "-".into();
    }
    let rank = (u128::from(total) * u128::from(q)).div_ceil(100) as u64;
    let mut seen = 0u64;
    for (i, n) in counts.iter().enumerate() {
        seen = seen.saturating_add(*n);
        if seen >= rank {
            return BUCKET_UPPER_MS[i]
                .map(|n| format!("≤{n}"))
                .unwrap_or_else(|| ">32768".into());
        }
    }
    "-".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_shape_for_both_clients() {
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/quality-hour.json")).unwrap();
        assert_eq!(
            serde_json::to_value(Counters::new().take(0, "host-test", "build-test")).unwrap(),
            expected
        );
    }
    #[test]
    fn worst_case_hour_stays_under_the_daily_bound() {
        let mut line = Counters::new().take(0, &"h".repeat(256), "build-test");
        for value in line.counters.values_mut() {
            *value = u64::MAX;
        }
        for counts in line.histograms.values_mut() {
            *counts = [u64::MAX; 17];
        }
        assert!((serde_json::to_vec(&line).unwrap().len() as u64 + 1) * 24 < MAX_FILE_BYTES);
    }
    #[test]
    fn presentation_is_once_and_hidden_frames_do_not_claim_freshness() {
        static COUNTS: Counters = Counters::new();
        let mut draw = Draw::with_metrics(Instant::now(), &COUNTS);
        draw.live(true);
        assert_eq!(
            COUNTS.take(0, "host-test", "build-test").histograms["reconnect_ms"]
                .iter()
                .sum::<u64>(),
            0
        );
        draw.live(false);
        draw.live(true);
        assert_eq!(
            COUNTS.take(0, "host-test", "build-test").histograms["reconnect_ms"]
                .iter()
                .sum::<u64>(),
            1
        );
        draw.key();
        draw.key();
        draw.frame(Instant::now(), 1);
        draw.frame(Instant::now(), 2);
        draw.presented(false, 0, 3, 1);
        let first = COUNTS.take(0, "host-test", "build-test");
        assert_eq!(first.histograms["first_frame_ms"].iter().sum::<u64>(), 1);
        assert_eq!(
            first.histograms["first_real_state_ms"].iter().sum::<u64>(),
            0
        );
        assert_eq!(first.histograms["key_to_redraw_ms"].iter().sum::<u64>(), 2);
        assert_eq!(first.histograms["freshness_ms"].iter().sum::<u64>(), 1);
        draw.presented(true, 1, 0, 3);
        draw.presented(true, 0, 3, 3);
        let second = COUNTS.take(0, "host-test", "build-test");
        assert_eq!(second.histograms["first_frame_ms"].iter().sum::<u64>(), 0);
        assert_eq!(
            second.histograms["first_real_state_ms"].iter().sum::<u64>(),
            1
        );
        assert_eq!(second.counters["first_screen_top_bar_flips"], 1);
        assert_eq!(second.counters["first_screen_placeholder_flips"], 1);
        assert_eq!(second.histograms["freshness_ms"].iter().sum::<u64>(), 0);
        for _ in 0..65 {
            draw.key();
        }
        assert_eq!(
            COUNTS.take(0, "host-test", "build-test").counters["measurement_overflow"],
            1
        );
    }
    #[test]
    fn boundaries() {
        for (i, upper) in BUCKET_UPPER_MS.iter().enumerate().take(16) {
            let ms = upper.unwrap();
            assert_eq!(bucket(Duration::from_millis(ms)), i);
            assert_eq!(bucket(Duration::from_micros(ms * 1000 + 1)), i + 1);
            assert_eq!(bucket(Duration::from_nanos(ms * 1_000_000 + 1)), i + 1);
        }
        assert_eq!(bucket(Duration::ZERO), 0);
        assert_eq!(bucket(Duration::MAX), 16);
    }
    #[test]
    fn line_has_only_fixed_measurements() {
        let counters = Counters::new();
        counters.add("panics", 1);
        counters.observe("key_to_redraw_ms", Duration::from_millis(10));
        let line = counters.take(123, "host-test", "build-test");
        let value = serde_json::to_value(&line).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 8);
        assert_eq!(value["histograms"]["key_to_redraw_ms"][4], 1);
        assert_eq!(value["counters"]["panics"], 1);
        assert_eq!(line.counters.len(), COUNTERS.len());
        assert_eq!(line.histograms.len(), HISTOGRAMS.len());
        assert!(COUNTERS.iter().all(|k| line.counters.contains_key(*k)));
        assert!(HISTOGRAMS.iter().all(|k| line.histograms.contains_key(*k)));
        assert!(!serde_json::to_string(&line).unwrap().contains("person/"));
        assert_eq!(
            counters.take(123, "host-test", "build-test").counters["panics"],
            0
        );
    }
    #[test]
    fn hourly_merging_rotation_and_bound() {
        let dir = tempfile::tempdir().unwrap();
        let counters = Counters::new();
        for h in 0..400 {
            counters.add("launches", 1);
            write_line(
                dir.path(),
                counters.take(h * 3600, "host-test", "build-test"),
            )
            .unwrap();
        }
        let rows: Vec<_> = files(dir.path())
            .iter()
            .flat_map(|p| read_lines(p))
            .collect();
        assert_eq!(rows.len(), 336);
        assert_eq!(rows.iter().map(|r| r.window_start).min(), Some(64 * 3600));
        assert!(files(dir.path()).len() <= 15);
        counters.add("launches", 2);
        write_line(
            dir.path(),
            counters.take(399 * 3600, "host-test", "build-test"),
        )
        .unwrap();
        assert_eq!(
            files(dir.path())
                .iter()
                .flat_map(|p| read_lines(p))
                .find(|r| r.window_start == 399 * 3600)
                .unwrap()
                .counters["launches"],
            3
        );
        assert!(
            files(dir.path())
                .iter()
                .all(|p| std::fs::metadata(p).unwrap().len() <= MAX_FILE_BYTES)
        );
    }
    #[test]
    fn a_late_shutdown_preserves_the_next_hour() {
        let dir = tempfile::tempdir().unwrap();
        let counters = Counters::new();
        counters.add("panics", 1);
        write_line(
            dir.path(),
            counters.take(24 * 3600, "host-test", "build-test"),
        )
        .unwrap();
        counters.add("launches", 1);
        write_line(
            dir.path(),
            counters.take(23 * 3600, "host-test", "build-test"),
        )
        .unwrap();
        let rows: Vec<_> = files(dir.path())
            .iter()
            .flat_map(|p| read_lines(p))
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .find(|r| r.window_start == 24 * 3600)
                .unwrap()
                .counters["panics"],
            1
        );
    }
    #[test]
    fn unavailable_directory_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "occupied").unwrap();
        assert!(
            write_line(
                &blocker.join("slo"),
                Counters::new().take(0, "host-test", "build-test")
            )
            .is_err()
        );
        let session = Session::start_in(Some(blocker.join("slo")));
        assert!(session.writer.is_none());
        drop(session);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
