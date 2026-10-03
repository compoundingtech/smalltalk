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
    /// Requests by the client that sent them, and by client and request kind.
    clients: BTreeMap<String, Sample>,
    client_requests: BTreeMap<String, Sample>,
    /// Wakes of the reconciler by what caused them.
    wakes: BTreeMap<String, Sample>,
    /// Writes a full reconcile pass made that an incremental pass would have missed.
    corrections: BTreeMap<String, Sample>,
    /// Item evaluations by whether an incremental pass would have run them.
    evaluations: BTreeMap<String, Sample>,
}
#[derive(Clone, Copy, PartialEq)]
enum Table {
    Requests,
    Queries,
    Clients,
    ClientRequests,
    Wakes,
    Corrections,
    Evaluations,
}
impl Bucket {
    fn table(&self, table: Table) -> &BTreeMap<String, Sample> {
        match table {
            Table::Requests => &self.requests,
            Table::Queries => &self.queries,
            Table::Clients => &self.clients,
            Table::ClientRequests => &self.client_requests,
            Table::Wakes => &self.wakes,
            Table::Corrections => &self.corrections,
            Table::Evaluations => &self.evaluations,
        }
    }
    fn table_mut(&mut self, table: Table) -> &mut BTreeMap<String, Sample> {
        match table {
            Table::Requests => &mut self.requests,
            Table::Queries => &mut self.queries,
            Table::Clients => &mut self.clients,
            Table::ClientRequests => &mut self.client_requests,
            Table::Wakes => &mut self.wakes,
            Table::Corrections => &mut self.corrections,
            Table::Evaluations => &mut self.evaluations,
        }
    }
}
/// Separates the client from the request kind in a `client_requests` label.
const CLIENT_KIND: char = '\u{1f}';
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
    fn sample(&mut self, now: Instant, table: Table, label: String) -> &mut Sample {
        self.prune(now);
        if self
            .buckets
            .back()
            .is_none_or(|(at, _)| now.duration_since(*at) >= Duration::from_secs(10))
        {
            self.buckets.push_back((now, Bucket::default()));
        }
        let map = self.buckets.back_mut().unwrap().1.table_mut(table);
        let label = if map.len() >= LABEL_LIMIT && !map.contains_key(&label) {
            "(other)".into()
        } else {
            label
        };
        map.entry(label).or_default()
    }
    fn record(&mut self, now: Instant, table: Table, label: String, duration: Duration, cpu: u64) {
        let entry = self.sample(now, table, label);
        let us = duration.as_micros().min(u64::MAX as u128) as u64;
        entry.count += 1;
        entry.total_us = entry.total_us.saturating_add(us);
        entry.max_us = entry.max_us.max(us);
        entry.cpu_us = entry.cpu_us.saturating_add(cpu / 1000);
    }
    /// Record one request under its kind and, when known, the client that sent it.
    fn record_request(
        &mut self,
        now: Instant,
        kind: &str,
        client: Option<&str>,
        duration: Duration,
    ) {
        self.record(now, Table::Requests, kind.into(), duration, 0);
        if let Some(client) = client {
            self.record(now, Table::Clients, client.into(), duration, 0);
            let label = format!("{client}{CLIENT_KIND}{kind}");
            self.record(now, Table::ClientRequests, label, duration, 0);
        }
    }
    fn totals(&self, table: Table) -> Vec<(&str, Sample)> {
        let mut totals = BTreeMap::<&str, Sample>::new();
        for (_, bucket) in &self.buckets {
            for (label, sample) in bucket.table(table) {
                let entry = totals.entry(label).or_default();
                entry.count += sample.count;
                entry.total_us += sample.total_us;
                entry.cpu_us += sample.cpu_us;
                entry.max_us = entry.max_us.max(sample.max_us);
            }
        }
        totals.into_iter().collect()
    }
    fn snapshot(&mut self, now: Instant) -> Value {
        self.prune(now);
        let row = |s: &Sample| {
            json!({"count":s.count,
                "total_ms":s.total_us as f64 / 1000.0, "max_ms":s.max_us as f64 / 1000.0,
                "mean_ms":s.total_us as f64 / s.count.max(1) as f64 / 1000.0,
                "cpu_ms":s.cpu_us as f64 / 1000.0})
        };
        let by_time = |table: Table| {
            let mut totals = self.totals(table);
            totals.sort_by(|a, b| b.1.total_us.cmp(&a.1.total_us).then_with(|| a.0.cmp(b.0)));
            totals
                .into_iter()
                .take(20)
                .map(|(label, s)| {
                    let mut row = row(&s);
                    row["kind"] = json!(label);
                    row
                })
                .collect::<Vec<_>>()
        };
        // Idle load is many cheap requests, so clients rank by count.
        let by_count = |table: Table| {
            let mut totals = self.totals(table);
            totals.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
            totals
                .into_iter()
                .take(20)
                .map(|(label, s)| {
                    let mut row = row(&s);
                    match label.split_once(CLIENT_KIND) {
                        Some((client, kind)) => {
                            row["client"] = json!(client);
                            row["kind"] = json!(kind);
                        }
                        None => row["client"] = json!(label),
                    }
                    row
                })
                .collect::<Vec<_>>()
        };
        // Request kinds are routes; background tasks share the table under plain names.
        let request_count = self
            .totals(Table::Requests)
            .iter()
            .filter(|(kind, _)| kind.starts_with('/'))
            .map(|(_, s)| s.count)
            .sum::<u64>();
        let window_seconds = self
            .buckets
            .front()
            .map(|(at, _)| now.duration_since(*at).as_secs().clamp(1, WINDOW.as_secs()))
            .unwrap_or(WINDOW.as_secs());
        let mut wakes = self.totals(Table::Wakes);
        wakes.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        let wakes = wakes
            .into_iter()
            .take(20)
            .map(|(cause, s)| json!({"cause": cause, "count": s.count}))
            .collect::<Vec<_>>();
        let corrections = self
            .totals(Table::Corrections)
            .into_iter()
            .map(|(item, s)| json!({"item": item, "count": s.count}))
            .collect::<Vec<_>>();
        let evaluations = self
            .totals(Table::Evaluations)
            .into_iter()
            .map(|(item, s)| json!({"item": item, "count": s.count, "cpu_ms": s.cpu_us as f64 / 1000.0}))
            .collect::<Vec<_>>();
        json!({"window_seconds":300,"requests":by_time(Table::Requests),"queries":by_time(Table::Queries),
            "request_count":request_count,"sampled_seconds":window_seconds,
            "clients":by_count(Table::Clients),"client_requests":by_count(Table::ClientRequests),
            "reconciler_wakes":wakes,"incremental_corrections":corrections,"incremental_evaluations":evaluations,
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
        Table::Queries,
        kind,
        duration,
        0,
    );
}
/// Record a finished request under its route and the client that sent it, when known.
pub fn record_request(kind: &str, client: Option<&str>, duration: Duration) {
    METER
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .record_request(Instant::now(), kind, client, duration);
}
/// Count one wake of the reconciler under its cause: the request and client that changed the graph,
/// with `detail` such as the claim kind it wrote, or `source` when no request was running.
pub fn record_wake(source: &str, detail: Option<&str>) {
    let mut label = match current() {
        Some(Charged { kind, client }) => match client {
            Some(client) => format!("{kind} · {client}"),
            None => kind,
        },
        None => source.to_owned(),
    };
    if let Some(detail) = detail {
        label.push_str(" · ");
        label.push_str(detail);
    }
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        Table::Wakes,
        label,
        Duration::ZERO,
        0,
    );
}
/// Count a write that a full reconcile pass made for `item` (an item kind, such as `mission-run`)
/// that an incremental pass would have skipped. Every one is a bug in what the incremental pass
/// tracks; see `doc/fleet/smalltalk/idle-cpu-incremental-design`.
pub fn record_correction(item: &str) {
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        Table::Corrections,
        item.to_owned(),
        Duration::ZERO,
        0,
    );
}
/// Count one evaluation of an item of kind `item`, under whether an incremental pass would have
/// run it (`needed`) or skipped it, with the CPU it cost.
pub fn record_evaluation(item: &str, needed: bool, cpu: Duration) {
    let label = format!("{item} · {}", if needed { "needed" } else { "skippable" });
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        Table::Evaluations,
        label,
        Duration::ZERO,
        u64::try_from(cpu.as_nanos()).unwrap_or(u64::MAX),
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
    let result = with_cpu(Some(kind), None, work);
    METER.get_or_init(Mutex::default).lock().unwrap().record(
        Instant::now(),
        Table::Requests,
        kind.into(),
        started.elapsed(),
        0,
    );
    result
}
/// The request kind and client a thread works for, so CPU spent on its behalf is charged to both.
#[derive(Clone)]
pub struct Charged {
    kind: String,
    client: Option<String>,
}
thread_local! {static CURRENT: RefCell<Option<Charged>> = const {RefCell::new(None)};}
pub fn current() -> Option<Charged> {
    CURRENT.with(|label| label.borrow().clone())
}
/// Run `work` charged to what another thread was charged to, such as a request's store work.
pub fn with_charged<T>(charged: Option<Charged>, work: impl FnOnce() -> T) -> T {
    match charged {
        Some(Charged { kind, client }) => with_cpu(Some(&kind), client.as_deref(), work),
        None => work(),
    }
}
pub fn with_cpu<T>(kind: Option<&str>, client: Option<&str>, work: impl FnOnce() -> T) -> T {
    let Some(kind) = kind else {
        return work();
    };
    if current().is_some() {
        return work();
    }
    let charged = Charged {
        kind: kind.into(),
        client: client.map(Into::into),
    };
    CURRENT.with(|label| *label.borrow_mut() = Some(charged.clone()));
    struct Charge {
        charged: Charged,
        started: u64,
    }
    impl Drop for Charge {
        fn drop(&mut self) {
            CURRENT.with(|label| label.borrow_mut().take());
            let cpu_us = thread_cpu_ns().saturating_sub(self.started) / 1000;
            let now = Instant::now();
            let Charged { kind, client } = &self.charged;
            let mut meter = METER.get_or_init(Mutex::default).lock().unwrap();
            let mut charge = |table, label| {
                let entry = meter.sample(now, table, label);
                entry.cpu_us = entry.cpu_us.saturating_add(cpu_us);
            };
            charge(Table::Requests, kind.clone());
            if let Some(client) = client {
                charge(Table::Clients, client.clone());
                charge(
                    Table::ClientRequests,
                    format!("{client}{CLIENT_KIND}{kind}"),
                );
            }
        }
    }
    let _charge = Charge {
        charged,
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
                Table::Queries,
                format!("SELECT column{} FROM claims", i % 400),
                Duration::from_micros(20),
                0,
            );
        }
        meter.record_request(
            now,
            "GET /v1/client/missions",
            None,
            Duration::from_millis(1500),
        );
        assert!(meter.buckets[0].1.queries.len() <= LABEL_LIMIT + 1);
        let report = meter.snapshot(now);
        assert_eq!(report["requests"][0]["count"], 1);
        assert!(report["clients"].as_array().unwrap().is_empty());
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
    fn requests_are_counted_by_client_and_ranked_by_count() {
        let mut meter = Meter::default();
        let now = Instant::now();
        for _ in 0..30 {
            meter.record_request(
                now,
                "/v1/messages/page",
                Some("agent/a · claude"),
                Duration::from_millis(2),
            );
        }
        meter.record_request(
            now,
            "/v1/doctor",
            Some("st3 doctor"),
            Duration::from_secs(2),
        );
        meter.record_request(now, "/v1/health", None, Duration::from_millis(1));
        let report = meter.snapshot(now);
        assert_eq!(report["request_count"], 32);
        assert_eq!(report["clients"][0]["client"], "agent/a · claude");
        assert_eq!(report["clients"][0]["count"], 30);
        assert_eq!(report["clients"][1]["client"], "st3 doctor");
        assert_eq!(report["client_requests"][0]["kind"], "/v1/messages/page");
        assert_eq!(report["client_requests"][0]["count"], 30);
        assert_eq!(report["clients"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn reconciler_wakes_are_counted_by_the_request_that_caused_them() {
        let before = snapshot();
        let count = |report: &Value, cause: &str| {
            report["reconciler_wakes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["cause"] == cause)
                .map_or(0, |row| row["count"].as_u64().unwrap())
        };
        with_cpu(Some("/v1/claims"), Some("agent/a · st3 claim"), || {
            record_wake("api", Some("message.read"));
        });
        record_wake("timer step-timeout", None);
        let after = snapshot();
        let cause = "/v1/claims · agent/a · st3 claim · message.read";
        assert_eq!(count(&after, cause) - count(&before, cause), 1);
        assert_eq!(
            count(&after, "timer step-timeout") - count(&before, "timer step-timeout"),
            1
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
