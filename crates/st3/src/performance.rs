//! Always-on, bounded five-minute operational accounting. No graph claims or disk writes.
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(300);
const LABEL_LIMIT: usize = 256;

#[derive(Clone, Default)]
struct Sample {
    count: u64,
    total_us: u64,
    max_us: u64,
    cpu_us: u64,
}
#[derive(Default)]
struct Bucket {
    requests: BTreeMap<String, Sample>,
    queries: BTreeMap<String, Sample>,
}
#[derive(Default)]
struct Meter {
    buckets: VecDeque<(Instant, Bucket)>,
}
static METER: OnceLock<Mutex<Meter>> = OnceLock::new();

impl Meter {
    fn prune(&mut self, now: Instant) {
        while self
            .buckets
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= WINDOW)
        {
            self.buckets.pop_front();
        }
    }
    fn sample(&mut self, now: Instant, query: bool, label: String) -> &mut Sample {
        self.prune(now);
        if self
            .buckets
            .back()
            .is_none_or(|(at, _)| now.duration_since(*at) >= Duration::from_secs(10))
        {
            self.buckets.push_back((now, Bucket::default()));
        }
        let bucket = &mut self.buckets.back_mut().unwrap().1;
        let map = if query {
            &mut bucket.queries
        } else {
            &mut bucket.requests
        };
        let label = if map.len() >= LABEL_LIMIT && !map.contains_key(&label) {
            "(other)".into()
        } else {
            label
        };
        map.entry(label).or_default()
    }
    fn record(&mut self, now: Instant, query: bool, label: String, duration: Duration, cpu: u64) {
        let entry = self.sample(now, query, label);
        let us = duration.as_micros().min(u64::MAX as u128) as u64;
        entry.count += 1;
        entry.total_us = entry.total_us.saturating_add(us);
        entry.max_us = entry.max_us.max(us);
        entry.cpu_us = entry.cpu_us.saturating_add(cpu / 1000);
    }
    fn snapshot(&mut self, now: Instant) -> Value {
        self.prune(now);
        let rows = |query: bool| {
            let mut totals = BTreeMap::<&str, Sample>::new();
            for (_, bucket) in &self.buckets {
                for (label, sample) in if query {
                    &bucket.queries
                } else {
                    &bucket.requests
                } {
                    let entry = totals.entry(label).or_default();
                    entry.count += sample.count;
                    entry.total_us += sample.total_us;
                    entry.cpu_us += sample.cpu_us;
                    entry.max_us = entry.max_us.max(sample.max_us);
                }
            }
            let mut totals = totals.into_iter().collect::<Vec<_>>();
            totals.sort_by(|a, b| b.1.total_us.cmp(&a.1.total_us).then_with(|| a.0.cmp(b.0)));
            totals
                .into_iter()
                .take(20)
                .map(|(label, s)| {
                    json!({"kind":label,"count":s.count,
                "total_ms":s.total_us as f64 / 1000.0, "max_ms":s.max_us as f64 / 1000.0,
                "mean_ms":s.total_us as f64 / s.count.max(1) as f64 / 1000.0,
                "cpu_ms":s.cpu_us as f64 / 1000.0})
                })
                .collect::<Vec<_>>()
        };
        json!({"window_seconds":300,"requests":rows(false),"queries":rows(true),
            "query_time_note":"Statement wall time includes row processing; concurrent times overlap."})
    }
}

/// Normalize expanded SQLite SQL. Never retain string literals, numbers, or comments.
fn query_kind(sql: &str) -> String {
    let mut chars = sql.chars().peekable();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if out.len() >= 500 {
            break;
        }
        if c == '\'' {
            out.push('?');
            while let Some(q) = chars.next() {
                if q == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        } else if c == '-' && chars.peek() == Some(&'-') {
            for q in chars.by_ref() {
                if q == '\n' {
                    break;
                }
            }
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(q) = chars.next() {
                if q == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else if c.is_ascii_digit() && !out.ends_with(|p: char| p.is_alphanumeric() || p == '_') {
            out.push('?');
            while chars
                .peek()
                .is_some_and(|q| q.is_ascii_hexdigit() || matches!(q, '.' | 'x' | 'X'))
            {
                chars.next();
            }
        } else if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_owned()
}
pub fn record_query(sql: &str, duration: Duration) {
    let kind = query_kind(sql);
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        true,
        kind,
        duration,
        0,
    );
}
pub fn record_request(kind: &str, duration: Duration) {
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        false,
        kind.into(),
        duration,
        0,
    );
}
pub fn snapshot() -> Value {
    METER
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .snapshot(Instant::now())
}
/// Background tasks are charged their own thread's CPU, including when file profiling is off.
pub fn task<T>(kind: &'static str, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = with_cpu(Some(kind), work);
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        false,
        kind.into(),
        started.elapsed(),
        0,
    );
    result
}
thread_local! {static CURRENT: RefCell<Option<String>> = const {RefCell::new(None)};}
pub fn current() -> Option<String> {
    CURRENT.with(|label| label.borrow().clone())
}
pub fn with_cpu<T>(kind: Option<&str>, work: impl FnOnce() -> T) -> T {
    let Some(kind) = kind else {
        return work();
    };
    if current().is_some() {
        return work();
    }
    CURRENT.with(|label| *label.borrow_mut() = Some(kind.into()));
    struct Charge<'a> {
        kind: &'a str,
        started: u64,
    }
    impl Drop for Charge<'_> {
        fn drop(&mut self) {
            CURRENT.with(|label| label.borrow_mut().take());
            let cpu = thread_cpu_ns().saturating_sub(self.started);
            let mut meter = METER.get_or_init(Mutex::default).lock().unwrap();
            let entry = meter.sample(Instant::now(), false, self.kind.into());
            entry.cpu_us = entry.cpu_us.saturating_add(cpu / 1000);
        }
    }
    let _charge = Charge {
        kind,
        started: thread_cpu_ns(),
    };
    work()
}
fn thread_cpu_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to a valid timespec, CLOCK_THREAD_CPUTIME_ID reads this thread only.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0 {
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
    #[test]
    fn performance_window_is_bounded_ranked_and_expires() {
        let mut meter = Meter::default();
        let now = Instant::now();
        for i in 0..250_000 {
            meter.record(
                now,
                true,
                format!("SELECT column{} FROM claims", i % 400),
                Duration::from_micros(20),
                0,
            );
        }
        meter.record(
            now,
            false,
            "GET /v1/client/missions".into(),
            Duration::from_millis(1500),
            0,
        );
        assert!(meter.buckets[0].1.queries.len() <= LABEL_LIMIT + 1);
        let report = meter.snapshot(now);
        assert_eq!(report["requests"][0]["count"], 1);
        assert_eq!(report["requests"][0]["max_ms"], 1500.0);
        assert_eq!(report["queries"].as_array().unwrap().len(), 20);
        assert!(
            meter.snapshot(now + WINDOW)["queries"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn queries_do_not_disclose_values() {
        assert_eq!(
            query_kind(
                "SELECT * FROM claims WHERE subject='agent/secret''name' AND store_index>123 -- private\n"
            ),
            "SELECT * FROM claims WHERE subject=? AND store_index>?"
        );
        assert_eq!(
            query_kind("SELECT /* private */ 0x123, 'secret'"),
            "SELECT ?, ?"
        );
    }
}
