//! One daemon's evaluation of its conditions, run by a background task every
//! [`EVALUATE_EVERY_MS`](super::EVALUATE_EVERY_MS): read each value it needs once, fold it into
//! each instance's tracker, write what changed, and wake owners of a transition.
//!
//! A write that fails is tried again on the next tick: the transition stays pending until its
//! state claim is written, and a wake until its message is, so neither is lost or sent twice.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::probe::{self, ProcessSampler};
use super::{ConditionDecl, Metric, Scope, Tracker, Transition, Window};
use crate::disk::DiskSpace;
use crate::store::{ConditionRecord, Store};

/// One background evaluator per daemon. Kernel reads and store work run on the blocking
/// pool; request handlers only read already recorded state.
pub fn spawn(
    store: std::sync::Arc<Store>,
    host: String,
    database: PathBuf,
    notify: std::sync::Arc<tokio::sync::Notify>,
    events: tokio::sync::watch::Sender<u64>,
) {
    tokio::spawn(async move {
        let probe = HostProbe::new(database, Box::new(crate::api::request_latency_windows));
        let mut evaluator = Evaluator::new(host, Box::new(probe));
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(super::EVALUATE_EVERY_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let tick_store = store.clone();
            let task = tokio::task::spawn_blocking(move || {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let result = evaluator.tick(&tick_store, now);
                (evaluator, result)
            })
            .await;
            match task {
                Ok((next, result)) => {
                    evaluator = next;
                    match result {
                        Ok(report) => {
                            if report.recorded > 0 || !report.messages.is_empty() {
                                notify.notify_one();
                                events.send_modify(|index| *index = index.wrapping_add(1));
                            }
                            for error in report.errors {
                                tracing::warn!(%error, "condition evaluation");
                            }
                        }
                        Err(error) => tracing::warn!(%error, "condition evaluation"),
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "condition evaluator stopped");
                    break;
                }
            }
        }
    });
}

/// How long a spend reading is reused: it reads a day of usage rollups.
const COST_EVERY_MS: u128 = 5 * 60_000;

/// Where values come from. The daemon reads the host; tests supply their own.
pub(crate) trait Probe: Send {
    /// Free space on each local filesystem, by mount point.
    fn filesystems(&mut self) -> BTreeMap<String, DiskSpace>;
    fn filesystem_of(&mut self, path: &str) -> Option<DiskSpace>;
    fn memory_available_percent(&mut self) -> Option<f64>;
    fn processes(&mut self, names: &[&str], now: u128) -> BTreeMap<String, probe::ProcessReading>;
    fn database_bytes(&mut self) -> Option<f64>;
    /// The request-latency report: each target's and route's 1m, 5m and 1h windows.
    fn slo_windows(&mut self) -> Value;
}

/// The daemon's own host.
pub(crate) struct HostProbe {
    database: PathBuf,
    processes: ProcessSampler,
    slo: Box<dyn Fn() -> Value + Send>,
}

impl HostProbe {
    pub fn new(database: PathBuf, slo: Box<dyn Fn() -> Value + Send>) -> Self {
        Self {
            database,
            processes: ProcessSampler::default(),
            slo,
        }
    }
}

impl Probe for HostProbe {
    fn filesystems(&mut self) -> BTreeMap<String, DiskSpace> {
        probe::filesystems()
    }

    fn filesystem_of(&mut self, path: &str) -> Option<DiskSpace> {
        probe::filesystem_of(path)
    }

    fn memory_available_percent(&mut self) -> Option<f64> {
        probe::read_memory_available_percent()
    }

    fn processes(&mut self, names: &[&str], now: u128) -> BTreeMap<String, probe::ProcessReading> {
        self.processes.sample(names, now)
    }

    fn database_bytes(&mut self) -> Option<f64> {
        probe::database_bytes(&self.database)
    }

    fn slo_windows(&mut self) -> Value {
        (self.slo)()
    }
}

/// What one tick did, for the log and for tests.
#[derive(Debug, Default, PartialEq)]
pub struct TickReport {
    pub evaluated: usize,
    pub recorded: usize,
    pub transitions: Vec<(String, String, Transition)>,
    pub messages: Vec<String>,
    pub errors: Vec<String>,
}

pub(crate) struct Evaluator {
    host: String,
    probe: Box<dyn Probe>,
    trackers: BTreeMap<(String, String), Tracker>,
    restored: bool,
    /// Transitions whose state claim is not written yet.
    unrecorded: BTreeMap<(String, String), Transition>,
    cost: Option<(u128, f64)>,
}

/// The values one tick read, each at most once.
#[derive(Default)]
struct Readings {
    filesystems: Option<BTreeMap<String, DiskSpace>>,
    memory: Option<Option<f64>>,
    processes: Option<BTreeMap<String, probe::ProcessReading>>,
    database: Option<Option<f64>>,
    slo: Option<Value>,
    claim_bytes: Option<Option<f64>>,
}

impl Evaluator {
    pub fn new(host: impl Into<String>, probe: Box<dyn Probe>) -> Self {
        Self {
            host: host.into(),
            probe,
            trackers: BTreeMap::new(),
            restored: false,
            unrecorded: BTreeMap::new(),
            cost: None,
        }
    }

    pub fn tick(&mut self, store: &Store, now: u128) -> Result<TickReport> {
        let mut report = TickReport::default();
        // Bring the heads up to date first: they are what readers and a restart read.
        for _ in 0..4 {
            if store.fold_condition_heads()? < 500 {
                break;
            }
        }
        if !self.restored {
            self.trackers = store.condition_trackers(&self.host)?;
            self.restored = true;
        }
        let decls = store
            .declared_conditions()?
            .into_iter()
            .filter_map(|condition| condition.decl.ok())
            .filter(|decl| decl.applies_to(&self.host))
            .collect::<Vec<_>>();
        let mut readings = Readings::default();
        if decls
            .iter()
            .any(|decl| decl.metric == Metric::DbGrowthBytesPerDay)
        {
            store.fold_condition_claim_bytes(now)?;
            readings.claim_bytes = Some(store.condition_claim_bytes_per_day(now)?);
        }
        let process_names = decls
            .iter()
            .filter_map(|decl| decl.process.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if !process_names.is_empty() {
            readings.processes = Some(self.probe.processes(&process_names, now));
        }
        let mut live = BTreeSet::new();
        for decl in &decls {
            for (instance, value) in self.values(store, decl, &mut readings, now) {
                if !value.is_finite() {
                    continue;
                }
                let key = (decl.subject(), instance.clone());
                live.insert(key.clone());
                report.evaluated += 1;
                let tracker = self.trackers.entry(key.clone()).or_default();
                if let Some(transition) = tracker.observe(decl, value, now) {
                    report
                        .transitions
                        .push((key.0.clone(), instance.clone(), transition));
                    self.unrecorded.insert(key.clone(), transition);
                }
                let transition = self.unrecorded.get(&key).copied();
                if !tracker.should_record(transition, now) {
                    continue;
                }
                match store.record_condition_state(&ConditionRecord {
                    decl,
                    host: &self.host,
                    instance: &instance,
                    tracker,
                    transition,
                    now,
                }) {
                    Ok(_) => {
                        tracker.mark_recorded(now);
                        report.recorded += 1;
                        self.unrecorded.remove(&key);
                    }
                    Err(error) => report
                        .errors
                        .push(format!("{} {instance}: {error:#}", key.0)),
                }
            }
        }
        // Instances of conditions that no longer apply here stop being tracked. One whose value
        // could not be read this tick keeps its state for the next.
        let declared = decls
            .iter()
            .map(ConditionDecl::subject)
            .collect::<BTreeSet<_>>();
        self.trackers
            .retain(|key, _| declared.contains(&key.0) || live.contains(key));
        for (key, tracker) in &mut self.trackers {
            if !live.contains(key) {
                tracker.interrupt_hold(now);
            }
        }
        // Fold freshly written claims before draining the durable notification queue.
        for _ in 0..4 {
            if store.fold_condition_heads()? < 500 {
                break;
            }
        }
        match store.flush_condition_notifications() {
            Ok(messages) => report.messages.extend(messages),
            Err(error) => report.errors.push(format!("notifications: {error:#}")),
        }
        Ok(report)
    }

    /// Each instance of `decl` on this host and its value now. An instance without a reading
    /// is left out, and keeps its state.
    fn values(
        &mut self,
        store: &Store,
        decl: &ConditionDecl,
        readings: &mut Readings,
        now: u128,
    ) -> Vec<(String, f64)> {
        let host = self.host.clone();
        let disk = |space: &DiskSpace| match decl.metric {
            Metric::DiskFreeBytes => space.available as f64,
            _ => probe::free_percent(space),
        };
        match decl.metric {
            Metric::DiskFreePercent | Metric::DiskFreeBytes => match &decl.path {
                Some(path) => self
                    .probe
                    .filesystem_of(path)
                    .map(|space| vec![(format!("{host}:{path}"), disk(&space))])
                    .unwrap_or_default(),
                None => readings
                    .filesystems
                    .get_or_insert_with(|| self.probe.filesystems())
                    .iter()
                    .map(|(mount, space)| (format!("{host}:{mount}"), disk(space)))
                    .collect(),
            },
            Metric::MemoryAvailablePercent => readings
                .memory
                .get_or_insert_with(|| self.probe.memory_available_percent())
                .map(|value| vec![(host, value)])
                .unwrap_or_default(),
            Metric::ProcessCpuCores | Metric::ProcessRssBytes => {
                let reading = readings
                    .processes
                    .as_ref()
                    .and_then(|processes| processes.get(decl.process.as_deref()?));
                let value = match decl.metric {
                    Metric::ProcessCpuCores => reading.and_then(|reading| reading.cpu_cores),
                    _ => reading.map(|reading| reading.rss_bytes),
                };
                value.map(|value| vec![(host, value)]).unwrap_or_default()
            }
            Metric::DbSizeBytes => readings
                .database
                .get_or_insert_with(|| self.probe.database_bytes())
                .map(|value| vec![(host, value)])
                .unwrap_or_default(),
            Metric::DbGrowthBytesPerDay => readings
                .claim_bytes
                .flatten()
                .map(|value| vec![(host, value)])
                .unwrap_or_default(),
            Metric::CostUsdPerDay => self
                .cost_per_day(store, now)
                .map(|value| vec![(host, value)])
                .unwrap_or_default(),
            Metric::DaemonCpuCores | Metric::SloBurnRate | Metric::SloP99Ms => {
                let windows = readings.slo.get_or_insert_with(|| self.probe.slo_windows());
                slo_value(windows, decl)
                    .map(|value| vec![(host, value)])
                    .unwrap_or_default()
            }
        }
    }

    /// Spend by seats on this host in the last day, in dollars, read at most every few minutes.
    fn cost_per_day(&mut self, store: &Store, now: u128) -> Option<f64> {
        if let Some((at, value)) = self.cost
            && now.saturating_sub(at) < COST_EVERY_MS
        {
            return Some(value);
        }
        let until = u64::try_from(now).ok()?;
        let rows = store
            .usage_period_rows(until.saturating_sub(24 * 3_600_000), until)
            .ok()?;
        let host = format!("host/{}", self.host);
        let micro = rows
            .iter()
            .filter(|row| row["host"].as_str() == Some(host.as_str()))
            .filter_map(|row| row["cost_microusd"].as_f64())
            .sum::<f64>();
        let value = micro / 1_000_000.0;
        self.cost = Some((now, value));
        Some(value)
    }
}

/// A route or daemon CPU condition's value, from the request-latency report.
pub fn slo_value(report: &Value, decl: &ConditionDecl) -> Option<f64> {
    let window = decl.window.as_str();
    let targets = report["targets"].as_array()?;
    if decl.metric == Metric::DaemonCpuCores {
        let cpu = targets.iter().find(|target| target["name"] == "cpu")?;
        return cpu["windows"][window]["cores"].as_f64();
    }
    debug_assert_eq!(decl.scope, Scope::Route);
    let route = decl.route.as_deref()?;
    let windows = targets
        .iter()
        .find(|target| target["name"] == route)
        .map(|target| &target["windows"])
        .or_else(|| {
            report["paths"]
                .as_array()?
                .iter()
                .find(|path| path["path"] == route)
                .map(|path| &path["windows"])
        });
    let Some(windows) = windows else {
        // A target with no samples yet has no burn; a route with none has no row at all.
        return (decl.metric == Metric::SloBurnRate
            && crate::slo::targets()
                .latency
                .iter()
                .any(|target| target.name == route))
        .then_some(0.0);
    };
    let window = &windows[window];
    let count = window["count"].as_u64().unwrap_or(0);
    match decl.metric {
        Metric::SloBurnRate => Some(window["over_target_share"].as_f64().unwrap_or(0.0) / 0.01),
        _ if count == 0 => None,
        _ => window["p99_ms"].as_f64(),
    }
}

impl Window {
    /// The window's span, for text.
    pub fn millis(self) -> u128 {
        match self {
            Self::OneMinute => 60_000,
            Self::FiveMinutes => 5 * 60_000,
            Self::OneHour => 3_600_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::parse_test_intent as parse_intent;
    use crate::model::IntentInput;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake {
        free: Arc<Mutex<BTreeMap<String, (u64, u64)>>>,
        cpu: Arc<Mutex<Option<f64>>>,
        slo: Arc<Mutex<Value>>,
    }

    impl Probe for Fake {
        fn filesystems(&mut self) -> BTreeMap<String, DiskSpace> {
            self.free
                .lock()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, (mount, (available, total)))| {
                    (
                        mount.clone(),
                        DiskSpace {
                            filesystem: index as u64,
                            available: *available,
                            total: *total,
                        },
                    )
                })
                .collect()
        }
        fn filesystem_of(&mut self, path: &str) -> Option<DiskSpace> {
            self.filesystems().remove(path)
        }
        fn memory_available_percent(&mut self) -> Option<f64> {
            None
        }
        fn processes(
            &mut self,
            names: &[&str],
            _now: u128,
        ) -> BTreeMap<String, probe::ProcessReading> {
            let cpu = *self.cpu.lock().unwrap();
            names
                .iter()
                .filter(|_| cpu.is_some())
                .map(|name| {
                    (
                        (*name).to_owned(),
                        probe::ProcessReading {
                            cpu_cores: cpu,
                            rss_bytes: 1.0,
                        },
                    )
                })
                .collect()
        }
        fn database_bytes(&mut self) -> Option<f64> {
            Some(1_000.0)
        }
        fn slo_windows(&mut self) -> Value {
            self.slo.lock().unwrap().clone()
        }
    }

    const SOURCE: &str = r#"version 2
condition "fleet/disk" {
  metric "disk.free-percent"
  scope "host"
  below 15
  recover 18
  for "2m"
  recover-for "1m"
  owner "agent/ops"
}
condition "fleet/collector-cpu" {
  metric "process.cpu-cores"
  scope "process"
  process "collector"
  host "alder"
  above 1.5
  recover 1
  for "1m"
  owner "person/ada"
}
condition "fleet/elsewhere" {
  metric "db.size-bytes"
  scope "member"
  host "birch"
  above 1
  for "0s"
  owner "agent/ops"
}
"#;

    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("claims.sqlite3"), "alder").unwrap();
        let intent = parse_intent(SOURCE, "alder").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: SOURCE.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "conditions",
                Some("person/ada"),
            )
            .unwrap();
        (directory, store)
    }

    const S: u128 = 1_000;

    #[test]
    fn owners_are_woken_once_on_entering_breach_and_once_on_recovery() {
        let (_directory, store) = store();
        let fake = Fake::default();
        fake.free
            .lock()
            .unwrap()
            .extend([("/".to_owned(), (50, 100)), ("/data".to_owned(), (10, 100))]);
        *fake.cpu.lock().unwrap() = Some(0.2);
        let mut evaluator = Evaluator::new("alder", Box::new(fake.clone()));
        let start = 1_760_000_000_000u128;
        let mut transitions = Vec::new();
        let mut messages = Vec::new();
        let mut run = |evaluator: &mut Evaluator, from: u128, to: u128| {
            let mut at = from;
            while at <= to {
                let report = evaluator.tick(&store, start + at * S).unwrap();
                assert!(report.errors.is_empty(), "{:?}", report.errors);
                transitions.extend(report.transitions);
                messages.extend(report.messages);
                at += 30;
            }
        };
        // Two minutes under 15% on /data enters breach; / is fine; the other host's condition
        // is never evaluated here.
        run(&mut evaluator, 0, 150);
        assert_eq!(
            transitions,
            [(
                "condition/fleet/disk".to_owned(),
                "alder:/data".to_owned(),
                Transition::Enter
            )]
        );
        assert_eq!(messages.len(), 1);
        let message = store
            .claims_for(&messages[0], Some("message.sent"))
            .unwrap();
        assert_eq!(message[0].body["fields"]["to"], "agent/ops");
        assert_eq!(
            message[0].body["fields"]["title"],
            "Condition breached: fleet/disk on alder:/data"
        );
        let content = message[0].body["fields"]["content"].as_str().unwrap();
        assert!(
            content.contains("disk.free-percent is 10%, below its threshold of 15%"),
            "{content}"
        );
        assert!(
            content.contains("Series: st conditions show fleet/disk"),
            "{content}"
        );
        assert!(
            !message[0].body["evidence"].as_array().unwrap().is_empty(),
            "the message cites its state claim"
        );

        // A restarted evaluator continues from the graph and does not announce it again.
        let mut evaluator = Evaluator::new("alder", Box::new(fake.clone()));
        run(&mut evaluator, 180, 600);
        assert_eq!(transitions.len(), 1);

        // Climbing to 17% is not recovery; holding 20% for a minute is, once.
        fake.free.lock().unwrap().insert("/data".into(), (17, 100));
        run(&mut evaluator, 630, 900);
        assert_eq!(transitions.len(), 1);
        fake.free.lock().unwrap().insert("/data".into(), (20, 100));
        run(&mut evaluator, 930, 1200);
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[1].2, Transition::Recover);
        assert_eq!(messages.len(), 2);

        // A person's breach messages no one: it is an alert on their home until it recovers.
        *fake.cpu.lock().unwrap() = Some(3.0);
        run(&mut evaluator, 1230, 1320);
        assert_eq!(transitions.len(), 3);
        assert_eq!(messages.len(), 2);
        let alerts = store
            .attention_snapshot(Some("person/ada"), start + 1320 * S)
            .unwrap();
        let alert = alerts.iter().find(|item| item.kind == "condition").unwrap();
        assert_eq!(
            alert.title,
            "Condition breached: fleet/collector-cpu on alder"
        );
        *fake.cpu.lock().unwrap() = Some(0.5);
        run(&mut evaluator, 1350, 1440);
        assert_eq!(transitions.len(), 4);
        assert!(
            store
                .attention_snapshot(Some("person/ada"), start + 1440 * S)
                .unwrap()
                .iter()
                .all(|item| item.kind != "condition")
        );

        let conditions = store.conditions().unwrap();
        let disk = conditions
            .iter()
            .find(|c| c.subject == "condition/fleet/disk")
            .unwrap();
        assert_eq!(
            disk.instances
                .iter()
                .map(|i| (i.instance.as_str(), i.phase.as_str()))
                .collect::<Vec<_>>(),
            [("alder:/", "clear"), ("alder:/data", "clear")]
        );
        let elsewhere = conditions
            .iter()
            .find(|c| c.subject == "condition/fleet/elsewhere")
            .unwrap();
        assert!(elsewhere.instances.is_empty());
    }

    #[test]
    fn a_missing_reading_interrupts_entry_and_recovery_holds() {
        let (_directory, store) = store();
        let fake = Fake::default();
        fake.free.lock().unwrap().insert("/".into(), (10, 100));
        let mut evaluator = Evaluator::new("alder", Box::new(fake.clone()));
        let start = 1_760_000_000_000u128;
        evaluator.tick(&store, start).unwrap();
        fake.free.lock().unwrap().clear();
        evaluator.tick(&store, start + 30_000).unwrap();
        fake.free.lock().unwrap().insert("/".into(), (10, 100));
        assert!(
            evaluator
                .tick(&store, start + 120_000)
                .unwrap()
                .transitions
                .is_empty()
        );
        assert_eq!(
            evaluator.tick(&store, start + 240_000).unwrap().transitions[0].2,
            Transition::Enter
        );
        fake.free.lock().unwrap().insert("/".into(), (20, 100));
        evaluator.tick(&store, start + 270_000).unwrap();
        fake.free.lock().unwrap().clear();
        evaluator.tick(&store, start + 300_000).unwrap();
        fake.free.lock().unwrap().insert("/".into(), (20, 100));
        assert!(
            evaluator
                .tick(&store, start + 330_000)
                .unwrap()
                .transitions
                .is_empty()
        );
        assert_eq!(
            evaluator.tick(&store, start + 390_000).unwrap().transitions[0].2,
            Transition::Recover
        );
    }

    #[test]
    fn a_steady_value_writes_rarely_and_a_flapping_one_at_most_once_a_period() {
        let (_directory, store) = store();
        let fake = Fake::default();
        fake.free.lock().unwrap().insert("/".into(), (50, 100));
        let mut evaluator = Evaluator::new("alder", Box::new(fake.clone()));
        let start = 1_760_000_000_000u128;
        let mut recorded = 0;
        // An hour, alternating 14% and 16% every 30 seconds: under the two-minute hold each time.
        for tick in 0..120u128 {
            let free = if tick % 2 == 0 { 14 } else { 16 };
            fake.free.lock().unwrap().insert("/".into(), (free, 100));
            let report = evaluator.tick(&store, start + tick * 30 * S).unwrap();
            assert!(report.transitions.is_empty());
            recorded += report.recorded;
        }
        // The process condition has no reading (no process), so only the disk writes: the first
        // sample, then at most one write each five minutes.
        assert!(recorded <= 13, "{recorded} writes in an hour");
        let mut steady = 0;
        fake.free.lock().unwrap().insert("/".into(), (50, 100));
        for tick in 120..240u128 {
            steady += evaluator
                .tick(&store, start + tick * 30 * S)
                .unwrap()
                .recorded;
        }
        assert_eq!(steady, 1, "a steady value writes once, when it changed");
    }

    #[test]
    fn slo_values_come_from_the_request_latency_windows() {
        let report = json!({
            "targets": [
                {"name": "person-read", "windows": {"1h": {"count": 1000, "p99_ms": 140.0, "over_target_share": 0.03}}},
                {"name": "write-ack", "windows": {"1h": {"count": 0, "p99_ms": 0.0, "over_target_share": 0.0}}},
                {"name": "cpu", "windows": {"5m": {"cores": 0.4, "measured_seconds": 300}}},
            ],
            "paths": [
                {"path": "GET /v1/client/now", "target": "person-read", "windows": {"5m": {"count": 3, "p99_ms": 80.0, "over_target_share": 0.0}}},
            ],
        });
        let decl = |metric: &str, route: Option<&str>, window: Option<&str>| {
            let mut children = vec![
                json!({"name": "metric", "arguments": [metric]}),
                json!({"name": "scope", "arguments": [if route.is_some() { "route" } else { "member" }]}),
                json!({"name": "above", "arguments": [1]}),
                json!({"name": "for", "arguments": ["5m"]}),
                json!({"name": "owner", "arguments": ["agent/speed"]}),
            ];
            if let Some(route) = route {
                children.push(json!({"name": "route", "arguments": [route]}));
            }
            if let Some(window) = window {
                children.push(json!({"name": "window", "arguments": [window]}));
            }
            crate::conditions::parse_condition(
                "condition/x",
                &json!({"name": "condition", "arguments": ["x"], "children": children}),
            )
            .unwrap()
        };
        let burn = slo_value(&report, &decl("slo.burn-rate", Some("person-read"), None)).unwrap();
        assert!((burn - 3.0).abs() < 1e-9, "{burn}");
        assert_eq!(
            slo_value(&report, &decl("slo.p99-ms", Some("person-read"), None)),
            Some(140.0)
        );
        assert_eq!(
            slo_value(&report, &decl("slo.p99-ms", Some("write-ack"), None)),
            None
        );
        assert_eq!(
            slo_value(&report, &decl("slo.burn-rate", Some("write-ack"), None)),
            Some(0.0)
        );
        assert_eq!(
            slo_value(
                &report,
                &decl("slo.p99-ms", Some("GET /v1/client/now"), Some("5m"))
            ),
            Some(80.0)
        );
        assert_eq!(
            slo_value(
                &report,
                &decl("slo.burn-rate", Some("GET /v1/unknown"), None)
            ),
            None
        );
        assert_eq!(
            slo_value(&report, &decl("daemon.cpu-cores", None, None)),
            Some(0.4)
        );
    }
}
