//! The load test: a production-sized generated store, served requests at the rates a busy fleet
//! host serves them, while the reconciler runs. It fails when a request's p99 latency or the
//! daemon's CPU goes over its budget, or more than [`WORSE`] past main's baseline.
//!
//! ```sh
//! ST_LOAD_GATE=1 TMPDIR=/var/tmp cargo test --release -p st3 --test integration \
//!     daemon_load:: -- --nocapture
//! ```
//!
//! The mix ([`MIX`]) is the request kinds and rates a busy host's daemon counted in its
//! performance report, the busiest of twelve five-minute windows on 2026-10-03: seats posting harness
//! events and claims, paging their mailboxes and reading their desired state, the replication
//! worker exporting and receiving exchanges with a peer, lease renewals, status and work reads,
//! and a person moving through stui. Thirty seats also hold event long-polls open, and 22 client
//! WebSockets subscribe to the agents roster together and stay subscribed. Only kinds and rates;
//! no contents. Roster snapshots have a 300 ms budget, including a separate connect-to-snapshot
//! measurement. Their HTTP correctness oracle runs afterward, not as a cache-warming pre-read.
//!
//! The daemon runs on its own runtime, and the load on another, so the CPU it reports is the
//! daemon's: the process's CPU less the load threads' (and the peer store's writer, which stands
//! in for another machine).
//!
//! - `ST_LOAD_GATE=1` runs it. A debug build skips it.
//! - `ST_LOAD_SCALE` sets the generated store's scale. The default, `1`, is the busy host's size.
//! - `ST_BENCH_DIR` keeps the generated store for the next run, as for `daemon_bench`.
//! - `ST_LOAD_SECONDS` sets how long the load runs. The default is 120.
//! - `ST_LOAD_BASELINE` names main's reports to compare with, a file or a directory of them; the
//!   comparison takes the worst of each. Without one only the budgets apply.
//! - `ST_LOAD_REPORT` writes the steady report there, for the next comparison; the explicit
//!   upgrade-under-load report is saved in its sibling `upgrade` directory. Both cases keep
//!   the same workload, budgets and baseline. Migration setup/completion is reported separately.
//! - `ST_LOAD_PROFILE=collections-v1` has each of the 22 subscribers also hold missions, work,
//!   attention, summary, glasses and arrangements windows on its socket, as an attached stui or
//!   phone does. Each window's first snapshot has the roster's 300 ms budget. The default,
//!   `agents-v1`, holds the agents roster only. A report compares only with main's reports of
//!   its own profile.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::api::AppState;
use st3::client::Client;
use st3::model::WorkRequest;
use st3::store::Store;
use tokio::sync::{Barrier, Notify, mpsc, watch};

use crate::daemon_bench::{
    FLEET, NODE, PEER, Subjects, claim_input, env_number, fleet_subjects, generated_stores,
    percentile, stub_pty,
};

/// Shared runners tolerate up to twice main's baseline p99 or average daemon CPU.
/// Absolute budgets and request/correctness checks still apply independently.
const WORSE: f64 = 2.0;

/// A p99 this close to the baseline passes whatever the ratio: a few milliseconds of noise.
const LATENCY_SLACK: Duration = Duration::from_millis(5);

/// Sparse p99s are observed maxima; tolerate bounded runner noise without exempting the path.
const SPARSE_LATENCY_SLACK: Duration = Duration::from_millis(50);
const LATENCY_SAMPLES: usize = 50;

/// A single main run cannot establish the shared runner's latency variation.
const BASELINE_RUNS: usize = 5;

/// The daemon's CPU, in cores, may differ from the baseline by this much whatever the ratio.
const CPU_SLACK: f64 = 0.05;

/// The daemon's average CPU over the run may not pass this many cores.
const CPU_BUDGET: f64 = 2.0;

/// Requests in flight at once before the load stops adding more; a daemon this far behind fails.
const IN_FLIGHT_LIMIT: usize = 256;

const LONG_POLL: &str = "seat event long poll";
const SEATS: usize = 30;

const ROSTER_SUBSCRIBERS: usize = 22;
const ROSTER_LIMIT: usize = 200;
const ROSTER_SNAPSHOT: &str = "agents roster snapshot";
const ROSTER_CONNECT_SNAPSHOT: &str = "agents roster connect+snapshot";
const ROSTER_BUDGET: Duration = Duration::from_millis(300);

/// The windows besides the agents roster that each subscriber holds in the collections profile.
const COLLECTION_WINDOWS: &[&str] =
    &["missions", "work", "attention", "summary", "glasses", "arrangements"];
/// The person whose collections the subscribers read.
const OPERATOR: &str = "person/bench-operator";

/// Which windows the subscribers hold. Reports of one profile never compare with another's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum LoadProfile {
    #[default]
    #[serde(rename = "agents-v1")]
    Agents,
    #[serde(rename = "collections-v1")]
    Collections,
}

impl LoadProfile {
    fn from_env() -> Self {
        match std::env::var("ST_LOAD_PROFILE").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("agents-v1") => Self::Agents,
            Ok("collections-v1") => Self::Collections,
            Ok(other) => panic!("ST_LOAD_PROFILE must be agents-v1 or collections-v1, not {other}"),
            Err(error) => panic!("ST_LOAD_PROFILE: {error}"),
        }
    }

    /// The windows each subscriber holds beside the agents roster.
    fn windows(self) -> &'static [&'static str] {
        match self {
            Self::Agents => &[],
            Self::Collections => COLLECTION_WINDOWS,
        }
    }
}

/// The names a collection window's first snapshot is timed under.
fn snapshot_names(collection: &str) -> [String; 2] {
    [format!("{collection} snapshot"), format!("{collection} connect+snapshot")]
}

/// The statements a cold full roster rebuild may run per agent card. Main ran about 23 per card
/// on 2026-10-08, reading each agent's inputs one agent at a time; reading them a chunk of
/// agents at a time, it runs 866 for 3,011 cards (0.29 per card).
const COLD_ROSTER_STATEMENTS_PER_CARD: f64 = 0.33;

/// Held by each test that measures the whole process, its CPU or its statements, so that one
/// does not count the other's work when both run in one test process.
static WHOLE_PROCESS: Mutex<()> = Mutex::new(());

/// One kind of request, how many the busy host served each second, and its p99 budget.
struct Load {
    name: &'static str,
    per_second: f64,
    budget: Duration,
}

const fn load(name: &'static str, per_second: f64, budget_ms: u64) -> Load {
    Load {
        name,
        per_second,
        budget: Duration::from_millis(budget_ms),
    }
}

/// The busy host's request mix. Each name is a request [`send`] knows how to make.
const MIX: &[Load] = &[
    load("harness event", 7.5, 250),
    load("seat mailbox page", 5.0, 250),
    load("claim", 4.5, 250),
    load("seat desired state", 3.9, 250),
    load("delivery hold", 2.9, 250),
    load("fleet membership", 1.4, 250),
    load("replication exchange", 0.8, 2_000),
    load("replication peer failure", 0.4, 250),
    load("seat status", 0.4, 250),
    load("seat work", 0.3, 250),
    load("lease renewal", 0.3, 250),
    load("replication wake", 0.2, 1_000),
    load("message send", 0.1, 250),
    load("person read", 0.5, 250),
    // Enough samples to measure usage separately from the rotating person-read mix.
    load("client usage", 0.5, 300),
];

/// What a person reads while moving through stui, one after another, and each read's p99 budget.
const PERSON_READS: &[(&str, u64)] = &[
    ("/v1/client/now", 250),
    // The agents list reads every agent's state: 0.6 to 2 s at this size already.
    ("/v1/client/agents", 4_000),
    ("/v1/client/missions", 250),
    ("/v1/client/attention", 250),
    ("/v1/client/messages", 250),
    ("/v1/client/work", 250),
    ("/v1/client/terminals", 250),
    ("/v1/mission-runs?mission=bench/fleet", 250),
    ("/v1/replication/status", 250),
];

/// The report a run writes and the next run compares with.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Report {
    #[serde(default)]
    profile: LoadProfile,
    scale: f64,
    claims: u64,
    seconds: f64,
    /// The daemon's average CPU over the run, in cores.
    daemon_cores: f64,
    #[serde(default)]
    long_poll_seats: usize,
    #[serde(default)]
    roster_subscribers: usize,
    #[serde(default)]
    roster_change_frames: usize,
    /// Rosters the daemon folded again card by card during the timed load, by why: each one
    /// is every card refolded rather than only the changed ones.
    #[serde(default)]
    roster_full_folds: BTreeMap<String, u64>,
    /// In the collections profile, the subscribers whose window of each other collection
    /// received a valid first snapshot, and the change frames each collection's windows applied.
    #[serde(default)]
    collection_subscribers: BTreeMap<String, usize>,
    #[serde(default)]
    collection_change_frames: BTreeMap<String, usize>,
    #[serde(default)]
    regime: String,
    #[serde(default)]
    actual_ci_checkout: Option<String>,
    #[serde(default)]
    event_migration: Option<st3::maintenance::EventMigrationReport>,
    #[serde(default)]
    migration_pending_at_load_start: bool,
    #[serde(default)]
    migration_pending_at_load_end: bool,
    #[serde(default)]
    fixture_legacy_reconstruction_ms: f64,
    #[serde(default)]
    store_open_ms: f64,
    #[serde(default)]
    current_dropped: usize,
    paths: BTreeMap<String, PathReport>,
    failed: BTreeMap<String, usize>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PathReport {
    count: usize,
    p50_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    budget_ms: f64,
}

struct Baseline {
    report: Report,
    runs: usize,
}

#[test]
fn the_daemon_keeps_its_budgets_under_a_busy_hosts_load() {
    if std::env::var_os("ST_LOAD_GATE").is_none() {
        println!("skipped: set ST_LOAD_GATE=1 to run the load test");
        return;
    }
    if cfg!(debug_assertions) {
        println!("skipped: a debug build is too slow to measure; run with cargo test --release");
        return;
    }
    let _whole_process = WHOLE_PROCESS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let scale = env_number("ST_LOAD_SCALE", 1.0_f64);
    let seconds = env_number("ST_LOAD_SECONDS", 120_u64);
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();

    let make_daemon = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("st3-daemon")
            .enable_all()
            .build()
            .unwrap()
    };
    let generation = make_daemon();
    let started = Instant::now();
    let (source, peer_source) = generation.block_on(generated_stores(&keep, scale));
    println!("store ready in {:.0}s", started.elapsed().as_secs_f64());
    drop(generation);
    let mut failures = Vec::new();
    for regime in [LoadRegime::Upgrade, LoadRegime::Steady] {
        // Separate runtimes ensure no reconciler/server from the first case affects the second.
        let daemon = make_daemon();
        let report = run(
            &daemon,
            scale,
            &source,
            &peer_source,
            Duration::from_secs(seconds),
            regime,
        );
        print(&report);
        if let Some(path) = std::env::var_os("ST_LOAD_REPORT") {
            let path = PathBuf::from(path);
            let path = if matches!(regime, LoadRegime::Upgrade) {
                let parent = path.parent().unwrap().join("upgrade");
                std::fs::create_dir_all(&parent).unwrap();
                parent.join(path.file_name().unwrap())
            } else {
                path
            };
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        failures.extend(
            report_failures(&report)
                .into_iter()
                .map(|failure| format!("{}: {failure}", regime.name())),
        );
        drop(daemon);
    }
    assert!(
        failures.is_empty(),
        "the daemon missed {} budgets:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// CPU the daemon's request and task kinds were charged in its performance window, by kind.
fn cpu_by_kind() -> BTreeMap<String, f64> {
    smallclaims::performance::snapshot()["requests"].as_array().into_iter().flatten()
        .filter_map(|row| Some((row["kind"].as_str()?.to_owned(), row["cpu_ms"].as_f64()?)))
        .collect()
}

/// A cold full rebuild of the agents roster on the generated store, alone on a quiet store: the
/// statements it runs and how long it takes, as a daemon start or a roster refresher's first fold
/// pays them. Its gates are those of [`the_daemon_keeps_its_budgets_under_a_busy_hosts_load`]:
///
/// ```sh
/// ST_LOAD_GATE=1 TMPDIR=/var/tmp cargo test --release -p st3 --features perf-load \
///     --test perf_load daemon_load::a_cold_roster_rebuild -- --nocapture
/// ```
#[test]
fn a_cold_roster_rebuild_reads_the_fleet_in_few_statements() {
    if std::env::var_os("ST_LOAD_GATE").is_none() {
        println!("skipped: set ST_LOAD_GATE=1 to run the load test");
        return;
    }
    if cfg!(debug_assertions) {
        println!("skipped: a debug build is too slow to measure; run with cargo test --release");
        return;
    }
    // The statement counter is the process's: the load test must not run meanwhile.
    let _whole_process = WHOLE_PROCESS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let scale = env_number("ST_LOAD_SCALE", 1.0_f64);
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();
    let generation = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let (source, _) = generation.block_on(generated_stores(&keep, scale));
    drop(generation);
    let work = tempfile::tempdir().unwrap();
    let database = work.path().join("claims.sqlite3");
    for suffix in ["", "-wal"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", database.display())).unwrap();
        }
    }
    let mut rounds = Vec::new();
    for _ in 0..3 {
        // A store just opened has no kept reductions, as at a daemon start.
        let store = Store::open(&database, NODE).unwrap();
        let index = store.index().unwrap();
        let before = smallclaims::sqlite::work::total();
        let started = Instant::now();
        let cards = st3::api::cold_agent_roster_rebuild(&store, index).unwrap();
        let elapsed = started.elapsed();
        let statements = (smallclaims::sqlite::work::total() - before).statements;
        println!(
            "cold roster rebuild: {cards} cards, {statements} statements \
             ({:.1} per card), {:.0} ms",
            statements as f64 / cards.max(1) as f64,
            elapsed.as_secs_f64() * 1_000.0
        );
        rounds.push((cards, statements));
    }
    let (cards, statements) = rounds[rounds.len() - 1];
    assert!(cards > 0, "the generated store has no agent cards");
    assert!(
        statements as f64 <= COLD_ROSTER_STATEMENTS_PER_CARD * cards as f64,
        "a cold roster rebuild ran {statements} statements for {cards} cards, more than \
         {COLD_ROSTER_STATEMENTS_PER_CARD} per card"
    );
}

fn report_failures(report: &Report) -> Vec<String> {
    let mut failures = absolute_failures(report);
    if let Some(baseline) = std::env::var_os("ST_LOAD_BASELINE") {
        match worst_of(Path::new(&baseline), report.profile) {
            Some(baseline) => failures.extend(compare(report, &baseline)),
            None => println!(
                "no baseline at {}; only the budgets apply",
                Path::new(&baseline).display()
            ),
        }
    }
    failures
}

fn absolute_failures(report: &Report) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, path) in &report.paths {
        if path.p99_ms > path.budget_ms {
            failures.push(format!(
                "{name}: p99 {:.1} ms is over its {} ms budget",
                path.p99_ms, path.budget_ms
            ));
        }
    }
    for load in MIX {
        if !report
            .paths
            .keys()
            .any(|name| name == load.name || name.starts_with(&format!("{} ", load.name)))
        {
            failures.push(format!("{}: never answered", load.name));
        }
    }
    if report.long_poll_seats != SEATS {
        failures.push(format!(
            "only {} of {SEATS} seats completed a long-poll",
            report.long_poll_seats
        ));
    }
    if report.roster_subscribers != ROSTER_SUBSCRIBERS {
        failures.push(format!(
            "only {} of {ROSTER_SUBSCRIBERS} subscribers received a correct agents snapshot",
            report.roster_subscribers
        ));
    }
    for name in [ROSTER_SNAPSHOT, ROSTER_CONNECT_SNAPSHOT] {
        if report.paths.get(name).map_or(0, |path| path.count) != ROSTER_SUBSCRIBERS {
            failures.push(format!(
                "{name}: expected {ROSTER_SUBSCRIBERS} correct snapshots"
            ));
        }
    }
    for &collection in report.profile.windows() {
        let correct = report.collection_subscribers.get(collection).copied().unwrap_or(0);
        if correct != ROSTER_SUBSCRIBERS {
            failures.push(format!(
                "only {correct} of {ROSTER_SUBSCRIBERS} subscribers received a valid {collection} snapshot"
            ));
        }
        for name in snapshot_names(collection) {
            if report.paths.get(&name).map_or(0, |path| path.count) != ROSTER_SUBSCRIBERS {
                failures.push(format!("{name}: expected {ROSTER_SUBSCRIBERS} timed snapshots"));
            }
        }
    }
    if report.daemon_cores > CPU_BUDGET {
        failures.push(format!(
            "the daemon used {:.2} cores, over its {CPU_BUDGET} budget",
            report.daemon_cores
        ));
    }
    // Count classified one-shot drops for diagnosis, but do not use them to waive
    // writer contention in the acceptance profile.
    if report.current_dropped != 0 {
        failures.push(format!(
            "{} current samples dropped on writer contention",
            report.current_dropped
        ));
    }
    let requests = report.paths.values().map(|path| path.count).sum::<usize>();
    let failed = report.failed.values().sum::<usize>();
    // A rare path's timeout must not disappear inside an overall error allowance.
    if failed > 0 {
        failures.push(format!(
            "{failed} of {requests} requests failed: {:?}",
            report.failed
        ));
    }
    failures
}

/// Main's reports at `path`, a report or a directory of them, combined into the worst of each:
/// one run's p99 on a shared runner moves by half or more from the next's, so a regression is
/// what passes the worst of several runs. Only reports of `profile` count: holding more windows
/// costs more, so another profile's numbers say nothing about a regression.
fn worst_of(path: &Path, profile: LoadProfile) -> Option<Baseline> {
    let files = if path.is_dir() {
        std::fs::read_dir(path)
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|file| {
                file.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    let reports = files
        .iter()
        .filter_map(|file| serde_json::from_slice::<Report>(&std::fs::read(file).ok()?).ok())
        .filter(|report| report.profile == profile)
        .collect::<Vec<_>>();
    let mut worst = Report::default();
    for report in &reports {
        worst.daemon_cores = worst.daemon_cores.max(report.daemon_cores);
        for (name, path) in &report.paths {
            let into = worst.paths.entry(name.clone()).or_insert(PathReport {
                count: path.count,
                ..PathReport::default()
            });
            into.count = into.count.min(path.count);
            into.p50_ms = into.p50_ms.max(path.p50_ms);
            into.p99_ms = into.p99_ms.max(path.p99_ms);
            into.max_ms = into.max_ms.max(path.max_ms);
        }
    }
    if !reports.is_empty() {
        println!(
            "comparing with the worst of {} reports of main",
            reports.len()
        );
    }
    (!reports.is_empty()).then_some(Baseline {
        report: worst,
        runs: reports.len(),
    })
}

/// Where any measured path or daemon CPU is more than [`WORSE`] past the baseline.
/// Latency needs several main runs; CPU averages the whole workload and can compare immediately.
fn compare(report: &Report, baseline: &Baseline) -> Vec<String> {
    let mut failures = Vec::new();
    if baseline.runs < BASELINE_RUNS {
        println!(
            "latency baseline warming up: {} of {BASELINE_RUNS} main reports; absolute latency budgets and CPU comparisons still apply",
            baseline.runs
        );
    }
    for (name, path) in &report.paths {
        if baseline.runs < BASELINE_RUNS {
            continue;
        }
        let Some(before) = baseline.report.paths.get(name) else {
            continue;
        };
        let slack = if path.count.min(before.count) < LATENCY_SAMPLES {
            SPARSE_LATENCY_SLACK
        } else {
            LATENCY_SLACK
        };
        if path.p99_ms > before.p99_ms * WORSE
            && path.p99_ms > before.p99_ms + slack.as_secs_f64() * 1e3
        {
            failures.push(format!(
                "{name}: p99 {:.1} ms is more than {:.0}% over main's {:.1} ms",
                path.p99_ms,
                (WORSE - 1.0) * 100.0,
                before.p99_ms
            ));
        }
    }
    if report.daemon_cores > baseline.report.daemon_cores * WORSE
        && report.daemon_cores > baseline.report.daemon_cores + CPU_SLACK
    {
        failures.push(format!(
            "the daemon used {:.2} cores, more than {:.0}% over main's {:.2}",
            report.daemon_cores,
            (WORSE - 1.0) * 100.0,
            baseline.report.daemon_cores
        ));
    }
    failures
}

#[test]
fn concurrent_roster_snapshots_use_the_budget_and_existing_baseline_comparison() {
    assert_eq!(ROSTER_SUBSCRIBERS, 22);
    assert_eq!(ROSTER_LIMIT, 200);
    let budgets = BTreeMap::new();
    let mut report = Report {
        roster_subscribers: ROSTER_SUBSCRIBERS,
        roster_change_frames: 7,
        ..Report::default()
    };
    let mut baseline = Baseline {
        report: Report::default(),
        runs: BASELINE_RUNS,
    };
    for name in [ROSTER_SNAPSHOT, ROSTER_CONNECT_SNAPSHOT] {
        assert_eq!(budget(&budgets, name), Duration::from_millis(300));
        baseline.report.paths.insert(
            name.into(),
            PathReport {
                count: ROSTER_SUBSCRIBERS,
                p99_ms: 100.0,
                ..PathReport::default()
            },
        );
        report.paths.insert(
            name.into(),
            PathReport {
                count: ROSTER_SUBSCRIBERS,
                p99_ms: 200.0,
                budget_ms: ROSTER_BUDGET.as_secs_f64() * 1_000.0,
                ..PathReport::default()
            },
        );
    }
    // Twice main passes the relative rule, including sparse roster samples.
    assert!(compare(&report, &baseline).is_empty());
    for path in report.paths.values_mut() {
        path.p99_ms = 201.0;
    }
    let failures = compare(&report, &baseline);
    assert_eq!(failures.len(), 2);
    for name in [ROSTER_SNAPSHOT, ROSTER_CONNECT_SNAPSHOT] {
        assert!(failures.iter().any(|failure| failure.starts_with(name)));
    }
    let encoded = serde_json::to_value(&report).unwrap();
    let decoded: Report = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.roster_subscribers, ROSTER_SUBSCRIBERS);
    assert_eq!(decoded.roster_change_frames, 7);
    let mut legacy = encoded;
    legacy.as_object_mut().unwrap().remove("roster_subscribers");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("roster_change_frames");
    let decoded: Report = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.roster_subscribers, 0);
    assert_eq!(decoded.roster_change_frames, 0);
}

#[test]
fn infrequent_reads_still_fail_on_a_baseline_regression() {
    let path = "person read /v1/client/agents";
    let mut baseline = Baseline {
        report: Report::default(),
        runs: BASELINE_RUNS,
    };
    baseline.report.paths.insert(
        path.into(),
        PathReport {
            count: 9,
            p99_ms: 1_000.0,
            budget_ms: 4_000.0,
            ..PathReport::default()
        },
    );
    let mut report = Report::default();
    report.paths.insert(
        path.into(),
        PathReport {
            count: 9,
            p99_ms: 2_100.0,
            budget_ms: 4_000.0,
            ..PathReport::default()
        },
    );
    // The request still passes its absolute budget, but misses the baseline limit.
    let failures = compare(&report, &baseline);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains(path));
    report.paths.get_mut(path).unwrap().p99_ms = 2_000.0;
    assert!(compare(&report, &baseline).is_empty());
}

#[test]
fn sparse_p99_tolerates_runner_noise_without_exempting_the_path() {
    let name = "sparse request";
    let mut report = Report::default();
    let mut baseline = Baseline {
        report: Report::default(),
        runs: BASELINE_RUNS,
    };
    // Observed failures across four PRs, plus the sparse tolerance's exact boundary.
    for (count, before, after) in [
        (13, 5.28, 12.53),
        (4, 29.24, 54.12),
        (7, 94.98, 137.34),
        (49, 29.0, 79.0),
    ] {
        baseline.report.paths.insert(
            name.into(),
            PathReport {
                count,
                p99_ms: before,
                ..PathReport::default()
            },
        );
        report.paths.insert(
            name.into(),
            PathReport {
                count,
                p99_ms: after,
                ..PathReport::default()
            },
        );
        assert!(compare(&report, &baseline).is_empty());
    }
    report.paths.get_mut(name).unwrap().p99_ms = 79.01;
    assert_eq!(compare(&report, &baseline).len(), 1);

    // Both sides need enough samples for the ordinary 5ms tolerance.
    report.paths.get_mut(name).unwrap().count = LATENCY_SAMPLES;
    report.paths.get_mut(name).unwrap().p99_ms = 61.0;
    assert!(compare(&report, &baseline).is_empty());
    baseline.report.paths.get_mut(name).unwrap().count = LATENCY_SAMPLES;
    assert_eq!(compare(&report, &baseline).len(), 1);
}

#[test]
fn latency_needs_five_main_runs_but_cpu_compares_during_bootstrap() {
    let name = "fleet membership";
    let mut baseline = Baseline {
        report: Report {
            daemon_cores: 1.0,
            ..Report::default()
        },
        runs: BASELINE_RUNS - 1,
    };
    baseline.report.paths.insert(
        name.into(),
        PathReport {
            count: 169,
            p99_ms: 28.28,
            ..PathReport::default()
        },
    );
    let mut report = Report {
        daemon_cores: 1.0,
        ..Report::default()
    };
    report.paths.insert(
        name.into(),
        PathReport {
            count: 169,
            p99_ms: 60.0,
            ..PathReport::default()
        },
    );
    assert!(compare(&report, &baseline).is_empty());
    report.daemon_cores = 2.0;
    assert!(compare(&report, &baseline).is_empty());
    report.daemon_cores = 2.01;
    let failures = compare(&report, &baseline);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("cores"));
    report.daemon_cores = 1.0;
    baseline.runs = BASELINE_RUNS;
    assert_eq!(compare(&report, &baseline).len(), 1);

    // The actual cache reader counts reports, not samples or request paths.
    let directory = tempfile::tempdir().unwrap();
    for run in 0..BASELINE_RUNS {
        std::fs::write(
            directory.path().join(format!("load-{run}.json")),
            serde_json::to_vec(&baseline.report).unwrap(),
        )
        .unwrap();
        assert_eq!(
            worst_of(directory.path(), LoadProfile::Agents)
                .unwrap()
                .runs,
            run + 1
        );
    }
}

#[test]
fn unchanged_work_tolerates_bounded_shared_runner_variation() {
    let mut baseline = Baseline {
        report: Report {
            daemon_cores: 1.19,
            ..Report::default()
        },
        runs: BASELINE_RUNS,
    };
    let mut report = Report {
        daemon_cores: 1.94,
        ..Report::default()
    };
    for (name, count, before, after) in [
        ("fleet membership", 169, 28.28, 55.2),
        ("seat desired state", 100, 2.7, 5.4),
        ("sparse request", 13, 5.28, 12.53),
    ] {
        baseline.report.paths.insert(
            name.into(),
            PathReport {
                count,
                p99_ms: before,
                ..PathReport::default()
            },
        );
        report.paths.insert(
            name.into(),
            PathReport {
                count,
                p99_ms: after,
                ..PathReport::default()
            },
        );
    }
    assert!(compare(&baseline.report, &baseline).is_empty());
    assert!(compare(&report, &baseline).is_empty());
    // A well-sampled route beyond twice main and the 5ms slack still fails.
    report.paths.get_mut("fleet membership").unwrap().p99_ms = 60.0;
    let failures = compare(&report, &baseline);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].starts_with("fleet membership"));
}

#[test]
fn relative_tolerance_keeps_every_absolute_budget_and_correctness_check() {
    let mut report = Report {
        daemon_cores: CPU_BUDGET,
        long_poll_seats: SEATS,
        roster_subscribers: ROSTER_SUBSCRIBERS,
        ..Report::default()
    };
    for (name, budget_ms, count) in MIX
        .iter()
        .map(|load| (load.name, load.budget.as_millis() as u64, 100))
        .chain([
            (ROSTER_SNAPSHOT, 300, ROSTER_SUBSCRIBERS),
            (ROSTER_CONNECT_SNAPSHOT, 300, ROSTER_SUBSCRIBERS),
        ])
    {
        report.paths.insert(
            name.into(),
            PathReport {
                count,
                p99_ms: 10.0,
                budget_ms: budget_ms as f64,
                ..PathReport::default()
            },
        );
    }
    for (path, budget_ms) in PERSON_READS {
        report.paths.insert(
            format!("person read {path}"),
            PathReport {
                count: 50,
                p99_ms: 10.0,
                budget_ms: *budget_ms as f64,
                ..PathReport::default()
            },
        );
    }
    assert!(absolute_failures(&report).is_empty());
    report.current_dropped = 1;
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("current samples dropped"))
    );
    report.current_dropped = 0;
    for name in report.paths.keys().cloned().collect::<Vec<_>>() {
        let path = report.paths.get_mut(&name).unwrap();
        path.p99_ms = path.budget_ms + 0.01;
        assert!(
            absolute_failures(&report)
                .iter()
                .any(|failure| failure.starts_with(&name))
        );
        report.paths.get_mut(&name).unwrap().p99_ms = 10.0;
    }
    report.daemon_cores = CPU_BUDGET + 0.01;
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("cores"))
    );
    report.daemon_cores = CPU_BUDGET;
    report.failed.insert("claim".into(), 1);
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("requests failed"))
    );
    report.failed.clear();
    report.long_poll_seats = SEATS - 1;
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("long-poll"))
    );
    report.long_poll_seats = SEATS;
    report.roster_subscribers = ROSTER_SUBSCRIBERS - 1;
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("subscribers"))
    );
    report.roster_subscribers = ROSTER_SUBSCRIBERS;
    report.paths.get_mut(ROSTER_SNAPSHOT).unwrap().count -= 1;
    assert!(
        absolute_failures(&report)
            .iter()
            .any(|failure| failure.contains("correct snapshots"))
    );
}

fn print(report: &Report) {
    println!(
        "regime: {}; migration pending at timed start/end: {}/{}; private fixture reconstruction {:.1}ms; Store::open {:.1}ms",
        report.regime,
        report.migration_pending_at_load_start,
        report.migration_pending_at_load_end,
        report.fixture_legacy_reconstruction_ms,
        report.store_open_ms
    );
    if let Some(migration) = &report.event_migration {
        println!(
            "event migration (including pauses/retries): {}",
            serde_json::to_string(migration).unwrap()
        );
    }

    println!(
        "\n== load test: {:?} profile, scale {}, {} claims, {:.0}s, daemon {:.2} cores",
        report.profile, report.scale, report.claims, report.seconds, report.daemon_cores
    );
    println!("current samples dropped on writer contention: {}", report.current_dropped);
    println!(
        "agents roster: {}/{} concurrent subscribers with correct snapshots; {} validated change frames; window limit {}",
        report.roster_subscribers, ROSTER_SUBSCRIBERS, report.roster_change_frames, ROSTER_LIMIT
    );
    let full_folds = report.roster_full_folds.values().sum::<u64>();
    println!(
        "agents roster full refolds: {full_folds} ({:.1}/min) {:?}",
        full_folds as f64 * 60.0 / report.seconds.max(1.0),
        report.roster_full_folds
    );
    for &collection in report.profile.windows() {
        println!(
            "{collection}: {}/{ROSTER_SUBSCRIBERS} subscribers with valid snapshots; {} validated change frames",
            report.collection_subscribers.get(collection).copied().unwrap_or(0),
            report.collection_change_frames.get(collection).copied().unwrap_or(0),
        );
    }
    println!(
        "{:<28} {:>7} {:>8} {:>8} {:>8} {:>8}",
        "request", "n", "p50 ms", "p99 ms", "max ms", "budget"
    );
    for (name, path) in &report.paths {
        println!(
            "{:<28} {:>7} {:>8.1} {:>8.1} {:>8.1} {:>8.0}",
            name, path.count, path.p50_ms, path.p99_ms, path.max_ms, path.budget_ms
        );
    }
    if !report.failed.is_empty() {
        println!("failed requests: {:?}", report.failed);
    }
}

#[test]
fn upgrade_reports_stay_separate_from_the_unchanged_steady_baseline() {
    let root = tempfile::tempdir().unwrap();
    let mut steady = Report {
        daemon_cores: 1.0,
        ..Report::default()
    };
    steady.paths.insert(
        "claim".into(),
        PathReport {
            p99_ms: 10.0,
            count: 100,
            ..PathReport::default()
        },
    );
    std::fs::write(
        root.path().join("main.json"),
        serde_json::to_vec(&steady).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("upgrade")).unwrap();
    let mut upgrade = steady;
    upgrade.daemon_cores = 99.0;
    upgrade.paths.get_mut("claim").unwrap().p99_ms = 99.0;
    std::fs::write(
        root.path().join("upgrade/main.json"),
        serde_json::to_vec(&upgrade).unwrap(),
    )
    .unwrap();
    let baseline = worst_of(root.path(), LoadProfile::Agents).unwrap();
    assert_eq!(baseline.runs, 1);
    assert_eq!(baseline.report.daemon_cores, 1.0);
    assert_eq!(baseline.report.paths["claim"].p99_ms, 10.0);
}

#[test]
fn a_report_compares_only_with_reports_of_its_own_profile() {
    let root = tempfile::tempdir().unwrap();
    // Reports written before profiles existed are the roster-only workload.
    let legacy = json!({"scale": 1.0, "claims": 1, "seconds": 1.0, "daemon_cores": 1.0,
        "paths": {}, "failed": {}});
    std::fs::write(root.path().join("legacy.json"), legacy.to_string()).unwrap();
    let collections = Report {
        profile: LoadProfile::Collections,
        daemon_cores: 3.0,
        ..Report::default()
    };
    std::fs::write(
        root.path().join("collections.json"),
        serde_json::to_vec(&collections).unwrap(),
    )
    .unwrap();
    let agents = worst_of(root.path(), LoadProfile::Agents).unwrap();
    assert_eq!((agents.runs, agents.report.daemon_cores), (1, 1.0));
    let held = worst_of(root.path(), LoadProfile::Collections).unwrap();
    assert_eq!((held.runs, held.report.daemon_cores), (1, 3.0));
    std::fs::remove_file(root.path().join("collections.json")).unwrap();
    assert!(worst_of(root.path(), LoadProfile::Collections).is_none());
}

#[test]
fn every_held_window_needs_a_timed_snapshot_per_subscriber_within_the_roster_budget() {
    let budgets = BTreeMap::new();
    let mut report = Report {
        profile: LoadProfile::Collections,
        roster_subscribers: ROSTER_SUBSCRIBERS,
        long_poll_seats: SEATS,
        ..Report::default()
    };
    for name in [ROSTER_SNAPSHOT, ROSTER_CONNECT_SNAPSHOT] {
        report.paths.insert(name.into(), PathReport { count: ROSTER_SUBSCRIBERS, ..PathReport::default() });
    }
    for load in MIX {
        report.paths.insert(load.name.into(), PathReport { count: 1, ..PathReport::default() });
    }
    for &collection in COLLECTION_WINDOWS {
        report.collection_subscribers.insert(collection.into(), ROSTER_SUBSCRIBERS);
        for name in snapshot_names(collection) {
            assert_eq!(budget(&budgets, &name), ROSTER_BUDGET);
            report.paths.insert(name, PathReport { count: ROSTER_SUBSCRIBERS, ..PathReport::default() });
        }
    }
    assert_eq!(absolute_failures(&report), Vec::<String>::new());
    report.collection_subscribers.insert("work".into(), ROSTER_SUBSCRIBERS - 1);
    report.paths.get_mut("missions snapshot").unwrap().count -= 1;
    assert_eq!(absolute_failures(&report).len(), 2);
    // The roster-only profile asks for no other window.
    report.profile = LoadProfile::Agents;
    assert_eq!(absolute_failures(&report), Vec::<String>::new());
}

/// Everything a request needs.
struct Context {
    client: Client,
    /// Client reads come from a person, as stui and the app make them.
    person: Client,
    store: Arc<Store>,
    peer: Arc<Store>,
    daemon: tokio::runtime::Handle,
    subjects: Subjects,
    /// Turns, so each request kind cycles through seats and reads.
    turns: AtomicUsize,
    current_dropped: AtomicUsize,
}

#[derive(Clone, Copy)]
enum LoadRegime {
    Upgrade,
    Steady,
}

impl LoadRegime {
    fn name(self) -> &'static str {
        match self {
            Self::Upgrade => "upgrade-under-load",
            Self::Steady => "steady-after-migration",
        }
    }
}

type MigrationTask =
    tokio::task::JoinHandle<anyhow::Result<st3::maintenance::EventMigrationReport>>;

fn start_event_migration(
    daemon: &tokio::runtime::Runtime,
    store: Arc<Store>,
    regime: LoadRegime,
) -> (
    Option<MigrationTask>,
    Option<st3::maintenance::EventMigrationReport>,
) {
    let migration = daemon.spawn(st3::maintenance::migrate_event_payloads(store.clone()));
    if matches!(regime, LoadRegime::Steady) {
        let report = daemon.block_on(migration).unwrap().unwrap();
        assert!(report.pending_at_start && report.completed);
        assert!(!store.event_payload_migration_pending().unwrap());
        (None, Some(report))
    } else {
        (Some(migration), None)
    }
}

#[test]
fn load_fixture_starts_the_shared_worker_and_steady_waits_for_completion() {
    let root = tempfile::tempdir().unwrap();
    let daemon = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for regime in [LoadRegime::Upgrade, LoadRegime::Steady] {
        let path = root.path().join(format!("{}.sqlite3", regime.name()));
        let store = Store::open(&path, NODE).unwrap();
        for number in 0..65 {
            store
                .append_claim(&st3::model::ClaimInput {
                    subject: format!("custom/test/migration-{number}"),
                    kind: "custom.test.recorded".into(),
                    actor: None,
                    fields: Default::default(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let before = store.events_after(0, None).unwrap();
        drop(store);
        prepare_legacy_event_fixture(&path);
        let store = Arc::new(Store::open(&path, NODE).unwrap());
        assert!(store.event_payload_migration_pending().unwrap());
        let (task, report) = start_event_migration(&daemon, store.clone(), regime);
        let report = match regime {
            LoadRegime::Upgrade => {
                assert!(
                    report.is_none(),
                    "upgrade overlaps the workload with the running worker"
                );
                daemon.block_on(task.unwrap()).unwrap().unwrap()
            }
            LoadRegime::Steady => {
                assert!(
                    task.is_none(),
                    "steady workload cannot start until the worker finishes"
                );
                assert!(!store.event_payload_migration_pending().unwrap());
                report.unwrap()
            }
        };
        assert!(report.pending_at_start && report.completed);
        assert_eq!((report.moved_rows, report.chunks), (65, 2));
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(store.events_after(0, None).unwrap()).unwrap()
        );
    }
}

/// Cached generated sources may already be schema 17. Reconstruct only the legacy event
/// table on this private fixture copy so the explicit upgrade case really has work to drain.
/// This fixture preparation is reported separately; it is not production upgrade cost.
fn prepare_legacy_event_fixture(path: &Path) -> f64 {
    let started = Instant::now();
    let connection = rusqlite::Connection::open(path).unwrap();
    let kind: String = connection
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='events'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    if kind == "view" {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE legacy_fixture_events(store_index INTEGER PRIMARY KEY,kind TEXT,subject TEXT,body TEXT);
             INSERT INTO legacy_fixture_events SELECT * FROM events;
             DROP VIEW events;
             DROP TABLE IF EXISTS local_event_payloads;
             DROP TABLE event_positions;
             ALTER TABLE legacy_fixture_events RENAME TO events;
             PRAGMA user_version=16;
             COMMIT;"
        ).unwrap();
    }
    started.elapsed().as_secs_f64() * 1_000.0
}

fn run(
    daemon: &tokio::runtime::Runtime,
    scale: f64,
    source: &Path,
    peer_source: &Path,
    duration: Duration,
    regime: LoadRegime,
) -> Report {
    let profile = LoadProfile::from_env();
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let database = root.join("state/claims.sqlite3");
    let peer_database = root.join("peer.sqlite3");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    for (from, to) in [(source, &database), (peer_source, &peer_database)] {
        for suffix in ["", "-wal"] {
            let from = PathBuf::from(format!("{}{suffix}", from.display()));
            if from.exists() {
                std::fs::copy(&from, format!("{}{suffix}", to.display())).unwrap();
            }
        }
    }
    // The peer stands in for another machine: its writer thread is load, not daemon.
    let before = threads();
    let peer = Arc::new(Store::open(&peer_database, PEER).unwrap());
    let peer_threads = threads()
        .difference(&before)
        .copied()
        .collect::<BTreeSet<_>>();

    let fixture_legacy_reconstruction_ms = prepare_legacy_event_fixture(&database);
    let opened = Instant::now();
    let store = Arc::new(Store::open(&database, NODE).unwrap());
    let store_open_ms = opened.elapsed().as_secs_f64() * 1_000.0;
    assert!(store.event_payload_migration_pending().unwrap());
    let (migration, mut event_migration) = start_event_migration(daemon, store.clone(), regime);
    store.bind_fleet(FLEET).ok();
    let claims = store.index().unwrap();
    let socket = root.join("st3.sock");
    let pty = stub_pty(root);
    let notify = Arc::new(Notify::new());
    let (event_notify, _events) = watch::channel(0_u64);
    let state = AppState {
        store: store.clone(),
        notify: notify.clone(),
        event_notify: event_notify.clone(),
        node: NODE.into(),
        state_dir: root.join("state"),
        pty_root: root.join("pty"),
        pty_binary: pty.clone(),
        fleet_id: Some(FLEET.into()),
        configured_peers: vec![PEER.into()],
        client_relay: None,
        native_session_home: Some(root.join("home")),
        planner_default: st3::model::PlannerSpec::default(),
    };
    let subjects = {
        let _entered = daemon.enter();
        st3::api::start_operation_report(&state);
        // As the daemon starts: it folds the roster once and keeps it published.
        st3::api::start_agent_roster(&state);
        st3::api::start_current_value_maintenance(&state);
        let server_socket = socket.clone();
        daemon.spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
        let subjects = fleet_subjects(&store, SEATS);
        // Every seat runs, so its driver may publish harness events.
        for seat in &subjects.seats {
            let mut running =
                claim_input("runtime.observed", &format!("load-{seat}-running"), 0, "");
            running.subject = seat.clone();
            running.actor = Some(seat.clone());
            running.fields.insert("status".into(), json!("running"));
            running
                .fields
                .insert("incarnation_id".into(), json!(runtime_of(seat)));
            store.append_claim(&running).unwrap();
        }
        subjects
    };
    while UnixStream::connect(&socket).is_err() {
        std::thread::sleep(Duration::from_millis(10));
    }
    // The reconciler runs as on a host that owns none of the store's members: it passes over the
    // whole store, as the busy host's does, and starts nothing.
    let reconciler = Arc::new(
        st3::reconcile::Reconciler::native(
            store.clone(),
            &root.join("state"),
            Some(&root.join("pty")),
            &pty,
            NODE.into(),
            socket.display().to_string(),
            notify,
            event_notify,
            None,
        )
        .unwrap(),
    );
    daemon.spawn(reconciler.supervise());

    let load = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("st3-load")
        // Load threads must outlive the run, so their CPU is still there to subtract at its end.
        .thread_keep_alive(Duration::from_secs(3_600))
        .enable_all()
        .build()
        .unwrap();
    // The declared agents whose desired state the seats' statuslines read.
    let mut subjects = subjects;
    subjects.agents = store
        .desired_subjects()
        .unwrap_or_default()
        .into_iter()
        .map(|desired| desired.subject)
        .filter(|subject| subject.starts_with("agent/bench/"))
        .collect();
    assert!(
        !subjects.agents.is_empty(),
        "the generated store declares no agents"
    );
    let context = Arc::new(Context {
        client: Client::unix(&socket),
        person: Client::unix_as(&socket, "person/bench-operator").unwrap(),
        store,
        peer,
        daemon: daemon.handle().clone(),
        subjects,
        turns: AtomicUsize::new(0),
        current_dropped: AtomicUsize::new(0),
    });
    // Let the daemon settle after opening: its first reconciler pass is not the load's.
    std::thread::sleep(Duration::from_secs(5));

    let timings = Arc::new(Mutex::new(BTreeMap::<String, Vec<Duration>>::new()));
    let failed = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let long_poll_seats = Arc::new(AtomicUsize::new(0));
    let mut roster_subscribers = 0;
    let roster_change_frames = Arc::new(AtomicUsize::new(0));
    let mut collection_subscribers = BTreeMap::<String, usize>::new();
    let collection_change_frames = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let migration_pending_at_load_start = context.store.event_payload_migration_pending().unwrap();
    if matches!(regime, LoadRegime::Steady) {
        assert!(!migration_pending_at_load_start);
    }
    let cpu_before = (process_cpu(), load_cpu(&peer_threads));
    let full_folds_before = context.store.agent_roster_chunked_assemblies();
    let cpu_by_kind_before = cpu_by_kind();
    let cursor = context.store.index().unwrap();
    let started = Instant::now();
    load.block_on(async {
        let mut tasks = Vec::new();
        let mut polls = Vec::new();
        let (roster_stop, stopped) = watch::channel(false);
        let (initial_done, initial_ready) = watch::channel(false);
        for (index, kind) in MIX.iter().enumerate() {
            let (context, timings, failed, in_flight, running) = (
                context.clone(),
                timings.clone(),
                failed.clone(),
                in_flight.clone(),
                running.clone(),
            );
            let mut initial_ready = initial_ready.clone();
            tasks.push(tokio::spawn(async move {
                // The first person read must not warm the shared roster before the simultaneous
                // snapshots. The rest of the fleet's writes and reads run throughout.
                if kind.name == "person read" {
                    let _ = initial_ready.changed().await;
                }
                let period = Duration::from_secs_f64(1.0 / kind.per_second);
                // Spread the kinds' first requests over their periods.
                tokio::time::sleep(period.mul_f64((index as f64 * 0.37) % 1.0)).await;
                let mut ticks = tokio::time::interval(period);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                while running.load(Ordering::Relaxed) {
                    ticks.tick().await;
                    if in_flight.load(Ordering::Relaxed) >= IN_FLIGHT_LIMIT {
                        *failed
                            .lock()
                            .unwrap()
                            .entry(format!("{}: daemon too far behind", kind.name))
                            .or_default() += 1;
                        continue;
                    }
                    // Requests go out on time however slow the last answer was, as a fleet's do.
                    let (context, timings, failed, in_flight) = (
                        context.clone(),
                        timings.clone(),
                        failed.clone(),
                        in_flight.clone(),
                    );
                    in_flight.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        let started = Instant::now();
                        let outcome = tokio::time::timeout(
                            Duration::from_secs(30),
                            send(&context, kind.name),
                        )
                        .await;
                        let took = started.elapsed();
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        match outcome {
                            Ok(Ok(label)) => timings
                                .lock()
                                .unwrap()
                                .entry(label.unwrap_or_else(|| kind.name.into()))
                                .or_default()
                                .push(took),
                            Ok(Err(error)) => {
                                *failed
                                    .lock()
                                    .unwrap()
                                    .entry(format!(
                                        "{}: {}",
                                        kind.name,
                                        error.chars().take(90).collect::<String>()
                                    ))
                                    .or_default() += 1
                            }
                            Err(_) => {
                                *failed
                                    .lock()
                                    .unwrap()
                                    .entry(format!("{}: timed out", kind.name))
                                    .or_default() += 1
                            }
                        }
                    });
                }
            }));
        }
        for seat in &context.subjects.seats {
            let (client, seat, timings, failed, running, long_poll_seats) = (
                context.client.clone(),
                seat.clone(),
                timings.clone(),
                failed.clone(),
                running.clone(),
                long_poll_seats.clone(),
            );
            polls.push(tokio::spawn(async move {
                let mut cursor = cursor;
                let mut answered = false;
                while running.load(Ordering::Relaxed) {
                    let path = format!(
                        "/v1/events/page?after={cursor}&subject={}&wait=true&timeout_ms=30000&limit=200",
                        urlencoding::encode(&seat)
                    );
                    let started = Instant::now();
                    match client.get::<Value>(&path).await {
                        Ok(events) => {
                            if !answered {
                                long_poll_seats.fetch_add(1, Ordering::Relaxed);
                                answered = true;
                            }
                            if let Some(next) = events["next_after"].as_u64() {
                                cursor = next;
                            }
                            timings
                                .lock()
                                .unwrap()
                                .entry(LONG_POLL.into())
                                .or_default()
                                .push(started.elapsed());
                        }
                        Err(error) => {
                            *failed
                                .lock()
                                .unwrap()
                                .entry(format!("{LONG_POLL}: {error}"))
                                .or_default() += 1;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }));
        }
        let barrier = Arc::new(Barrier::new(ROSTER_SUBSCRIBERS));
        let (snapshots, mut initial) =
            mpsc::channel(ROSTER_SUBSCRIBERS * (1 + profile.windows().len()));
        let mut subscribers = Vec::new();
        for subscriber in 0..ROSTER_SUBSCRIBERS {
            let client = st3_client::Client::unix_as(&socket, OPERATOR);
            let (barrier, stopped, snapshots, failed, changes, collection_changes) = (
                barrier.clone(),
                stopped.clone(),
                snapshots.clone(),
                failed.clone(),
                roster_change_frames.clone(),
                collection_change_frames.clone(),
            );
            subscribers.push(tokio::spawn(async move {
                if let Err(error) = roster_subscriber(
                    client, subscriber, barrier, stopped, snapshots, changes,
                    collection_changes, profile,
                )
                .await
                {
                    *failed
                        .lock()
                        .unwrap()
                        .entry(format!("{ROSTER_SNAPSHOT}: subscriber {subscriber}: {error}"))
                        .or_default() += 1;
                }
            }));
        }
        drop(snapshots);
        let mut captured = Vec::with_capacity(ROSTER_SUBSCRIBERS);
        while let Some(snapshot) = initial.recv().await {
            if snapshot.collection == "agents" {
                captured.push(snapshot);
                continue;
            }
            // The subscriber validated its typed rows and window; only the roster has an HTTP
            // oracle whose membership holds still under this load.
            *collection_subscribers.entry(snapshot.collection.into()).or_default() += 1;
            let [subscription, connection] = snapshot_names(snapshot.collection);
            let mut timings = timings.lock().unwrap();
            timings.entry(subscription).or_default().push(snapshot.subscription);
            timings.entry(connection).or_default().push(snapshot.connection);
        }
        // Capture every initial frame before reading the oracle: HTTP and WS share the roster
        // projection. Compare stable membership/declaration fields, not concurrently changing
        // activity, usage, lease or presence fields.
        let oracle = context
            .person
            .get::<Value>(&format!("/v1/client/agents?limit={ROSTER_LIMIT}"))
            .await
            .map_err(|error| error.to_string());
        for name in [ROSTER_SNAPSHOT, ROSTER_CONNECT_SNAPSHOT] {
            timings.lock().unwrap().entry(name.into()).or_default();
        }
        match oracle {
            Ok(oracle) => {
                for snapshot in captured {
                    match roster_matches_oracle(&snapshot.frame, &oracle) {
                        Ok(()) => {
                            roster_subscribers += 1;
                            let mut timings = timings.lock().unwrap();
                            timings.get_mut(ROSTER_SNAPSHOT).unwrap().push(snapshot.subscription);
                            timings
                                .get_mut(ROSTER_CONNECT_SNAPSHOT)
                                .unwrap()
                                .push(snapshot.connection);
                        }
                        Err(error) => {
                            *failed
                                .lock()
                                .unwrap()
                                .entry(format!("{ROSTER_SNAPSHOT}: {error}"))
                                .or_default() += 1;
                        }
                    }
                }
            }
            Err(error) => {
                *failed
                    .lock()
                    .unwrap()
                    .entry(format!("{ROSTER_SNAPSHOT}: HTTP oracle: {error}"))
                    .or_default() += 1;
            }
        }
        initial_done.send_replace(true);
        tokio::time::sleep_until(tokio::time::Instant::from_std(started + duration)).await;
        running.store(false, Ordering::Relaxed);
        // Cancelling outstanding quiet waits keeps their deliberate timeout outside the CPU
        // drain window. Otherwise up to 30 idle seconds would dilute the daemon's average CPU.
        for poll in polls {
            poll.abort();
            let _ = poll.await;
        }
        roster_stop.send_replace(true);
        for subscriber in subscribers {
            if let Err(error) = subscriber.await {
                *failed
                    .lock()
                    .unwrap()
                    .entry(format!("{ROSTER_SNAPSHOT}: subscriber task: {error}"))
                    .or_default() += 1;
            }
        }
        for task in tasks {
            let _ = task.await;
        }
        // Answers still due count; the CPU window closes once they are in.
        let deadline = Instant::now() + Duration::from_secs(30);
        while in_flight.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let cpu_after = (process_cpu(), load_cpu(&peer_threads));
    let mut cpu_by_kind = cpu_by_kind().into_iter()
        .map(|(kind, ms)| (ms - cpu_by_kind_before.get(&kind).copied().unwrap_or(0.0), kind))
        .filter(|(ms, _)| *ms > 0.0)
        .collect::<Vec<_>>();
    cpu_by_kind.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("daemon CPU during the load by kind (ms, top 12 of the 20 slowest kinds): {}",
        cpu_by_kind.iter().take(12).map(|(ms, kind)| format!("{kind} {ms:.0}"))
            .collect::<Vec<_>>().join(", "));
    let roster_full_folds = context.store.agent_roster_chunked_assemblies().into_iter()
        .map(|(why, count)| (why.clone(), count - full_folds_before.get(&why).copied().unwrap_or(0)))
        .filter(|(_, count)| *count > 0)
        .collect::<BTreeMap<_, _>>();
    let daemon_cpu = (cpu_after.0 - cpu_before.0) - (cpu_after.1 - cpu_before.1);

    let migration_pending_at_load_end = context.store.event_payload_migration_pending().unwrap();
    if let Some(migration) = migration {
        event_migration = Some(daemon.block_on(async {
            tokio::time::timeout(Duration::from_secs(600), migration)
                .await
                .expect("upgrade worker completes, even if it outlives the timed load")
                .unwrap()
                .unwrap()
        }));
    }
    assert!(event_migration.as_ref().unwrap().completed);
    assert!(!context.store.event_payload_migration_pending().unwrap());
    let timings = std::mem::take(&mut *timings.lock().unwrap());
    let budgets = MIX
        .iter()
        .map(|load| (load.name, load.budget))
        .collect::<BTreeMap<_, _>>();
    let paths = timings
        .into_iter()
        .map(|(name, samples)| {
            let millis = |duration: Duration| duration.as_secs_f64() * 1e3;
            let report = PathReport {
                count: samples.len(),
                p50_ms: millis(percentile(&samples, 50)),
                p99_ms: millis(percentile(&samples, 99)),
                max_ms: millis(samples.iter().max().copied().unwrap_or_default()),
                budget_ms: millis(budget(&budgets, &name)),
            };
            (name, report)
        })
        .collect();
    load.shutdown_timeout(Duration::from_secs(5));
    let failed = std::mem::take(&mut *failed.lock().unwrap());
    let collection_change_frames = std::mem::take(&mut *collection_change_frames.lock().unwrap());
    Report {
        profile,
        scale,
        claims,
        seconds: elapsed,
        daemon_cores: daemon_cpu / elapsed,
        long_poll_seats: long_poll_seats.load(Ordering::Relaxed),
        roster_subscribers,
        roster_change_frames: roster_change_frames.load(Ordering::Relaxed),
        roster_full_folds,
        collection_subscribers,
        collection_change_frames,
        regime: regime.name().into(),
        actual_ci_checkout: std::env::var("GITHUB_SHA").ok(),
        event_migration,
        migration_pending_at_load_start,
        migration_pending_at_load_end,
        fixture_legacy_reconstruction_ms,
        store_open_ms,
        current_dropped: context.current_dropped.load(Ordering::Relaxed),
        paths,
        failed,
    }
}

struct WindowSnapshot {
    collection: &'static str,
    frame: Value,
    subscription: Duration,
    connection: Duration,
}

/// One subscriber's window: what it holds, its fence, and when it subscribed.
struct HeldWindow {
    collection: &'static str,
    rows: BTreeSet<String>,
    index: Option<u64>,
    subscribed: Instant,
}

#[allow(clippy::too_many_arguments)]
async fn roster_subscriber(
    client: st3_client::Client,
    subscriber: usize,
    barrier: Arc<Barrier>,
    mut stopped: watch::Receiver<bool>,
    snapshots: mpsc::Sender<WindowSnapshot>,
    changes: Arc<AtomicUsize>,
    collection_changes: Arc<Mutex<BTreeMap<String, usize>>>,
    profile: LoadProfile,
) -> Result<(), String> {
    // Start handshakes together, then send subscriptions together once every handshake has
    // completed (or failed). Even a failed connector reaches the second barrier.
    barrier.wait().await;
    let connection = Instant::now();
    let opened = tokio::time::timeout(Duration::from_secs(30), client.collection_stream()).await;
    barrier.wait().await;
    let mut stream = opened
        .map_err(|_| "WebSocket handshake timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    let outcome = async {
        let initial_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut windows = BTreeMap::new();
        // The roster first, then every other window this profile holds, each timed from its
        // own subscribe command, as a client opening several tabs sends them.
        for collection in std::iter::once("agents").chain(profile.windows().iter().copied()) {
            let id = if collection == "agents" {
                format!("load-roster-{subscriber}")
            } else {
                format!("load-{collection}-{subscriber}")
            };
            let subscribed = Instant::now();
            let sent = match collection {
                "summary" => stream.subscribe(&id, collection, 1, None, None).await,
                "glasses" => stream.subscribe_glasses(&id).await,
                "arrangements" => {
                    stream.subscribe_arrangements(&id, OPERATOR, ROSTER_LIMIT, None).await
                }
                _ => stream.subscribe(&id, collection, ROSTER_LIMIT, None, None).await,
            };
            sent.map_err(|error| format!("{collection}: {error}"))?;
            windows.insert(
                id,
                HeldWindow { collection, rows: BTreeSet::new(), index: None, subscribed },
            );
        }
        let mut awaiting = windows.len();
        // The collector can finish once every first snapshot is in, without waiting for the
        // held subscriptions to close.
        let mut snapshots = Some(snapshots);
        loop {
            tokio::select! {
                result = stopped.changed() => {
                    result.map_err(|_| "shutdown signal disappeared".to_owned())?;
                    if *stopped.borrow() {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(initial_deadline), if awaiting > 0 => {
                    let missing = windows.values().filter(|window| window.index.is_none())
                        .map(|window| window.collection).collect::<Vec<_>>();
                    return Err(format!("initial snapshot timed out: {missing:?}"));
                }
                frame = stream.next() => {
                    let frame = frame
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| "WebSocket closed during the workload".to_owned())?;
                    let id = frame["id"].as_str().unwrap_or_default().to_owned();
                    let window = windows.get_mut(&id)
                        .ok_or_else(|| format!("frame for no held window: {frame}"))?;
                    let initial = window.index.is_none();
                    let next = apply_window_frame(window.collection, &frame, &id, initial, &mut window.rows)?;
                    if window.index.is_some_and(|index| next < index) {
                        return Err(format!("{} snapshot index moved backward", window.collection));
                    }
                    window.index = Some(next);
                    if initial {
                        let sender = snapshots.as_ref().expect("a first snapshot still awaited");
                        sender
                            .send(WindowSnapshot {
                                collection: window.collection,
                                frame,
                                subscription: window.subscribed.elapsed(),
                                connection: connection.elapsed(),
                            })
                            .await
                            .map_err(|_| "snapshot collector stopped".to_owned())?;
                        awaiting -= 1;
                        if awaiting == 0 {
                            snapshots = None;
                        }
                    } else if window.collection == "agents" {
                        changes.fetch_add(1, Ordering::Relaxed);
                    } else {
                        *collection_changes.lock().unwrap()
                            .entry(window.collection.into()).or_default() += 1;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    tokio::time::timeout(Duration::from_secs(1), stream.close())
        .await
        .map_err(|_| "WebSocket close timed out".to_owned())?;
    outcome
}

/// Validate real agent cards, not just any frame that happened to answer the subscription.
fn roster_row_ids(items: &Value) -> Result<BTreeSet<String>, String> {
    let items = items
        .as_array()
        .ok_or_else(|| "roster rows are not an array".to_owned())?;
    let mut ids = BTreeSet::new();
    for item in items {
        let agent = <st3_client::Agent as serde::Deserialize>::deserialize(item)
            .map_err(|error| format!("invalid agent card: {error}"))?;
        if item["kind"] != "agent" || !agent.header.id.starts_with("agent/") || agent.name.is_empty() {
            return Err("roster row is not a named agent".into());
        }
        if !ids.insert(agent.header.id) {
            return Err("roster contains duplicate agent rows".into());
        }
    }
    Ok(ids)
}

/// Validate typed rows of the windows the load holds: real agent, mission and work cards, and an
/// ID for every other collection's row.
fn window_row_ids(collection: &str, items: &Value) -> Result<BTreeSet<String>, String> {
    if collection == "agents" {
        return roster_row_ids(items);
    }
    let items = items
        .as_array()
        .ok_or_else(|| format!("{collection} rows are not an array"))?;
    let mut ids = BTreeSet::new();
    for item in items {
        let id = match collection {
            "missions" => <st3_client::Mission as serde::Deserialize>::deserialize(item)
                .map_err(|error| format!("invalid mission card: {error}"))?
                .header
                .id,
            "work" => <st3_client::Work as serde::Deserialize>::deserialize(item)
                .map_err(|error| format!("invalid work card: {error}"))?
                .header
                .id,
            _ => item["id"].as_str().unwrap_or_default().to_owned(),
        };
        if id.is_empty() {
            return Err(format!("{collection} row has no ID"));
        }
        if !ids.insert(id) {
            return Err(format!("{collection} contains duplicate rows"));
        }
    }
    Ok(ids)
}

/// Reconstruct each subscriber's window as changes arrive; invalid removals/order are failures.
fn apply_roster_frame(
    frame: &Value,
    id: &str,
    initial: bool,
    rows: &mut BTreeSet<String>,
) -> Result<u64, String> {
    apply_window_frame("agents", frame, id, initial, rows)
}

fn apply_window_frame(
    collection: &str,
    frame: &Value,
    id: &str,
    initial: bool,
    rows: &mut BTreeSet<String>,
) -> Result<u64, String> {
    let kind = if initial { "snapshot" } else { "changes" };
    if frame["kind"] != kind || frame["id"] != id || frame["collection"] != collection {
        return Err(format!("expected {collection} {kind} for {id}, received {frame}"));
    }
    let snapshot = <st3_client::Snapshot as serde::Deserialize>::deserialize(&frame["snapshot"])
        .map_err(|error| format!("invalid {collection} snapshot fence: {error}"))?;
    if frame["has_more"].as_bool().is_none() {
        return Err(format!("{collection} has no pagination flag"));
    }
    let order: Vec<String> =
        <Vec<String> as serde::Deserialize>::deserialize(&frame["order"])
            .map_err(|error| format!("invalid {collection} order: {error}"))?;
    let ordered = order.iter().cloned().collect::<BTreeSet<_>>();
    if order.len() > ROSTER_LIMIT || ordered.len() != order.len() {
        return Err(format!("{collection} order exceeds its window or contains duplicates"));
    }
    if initial {
        *rows = window_row_ids(collection, &frame["items"])?;
        // The generated store has agents, missions and open work; the rest may be empty.
        if rows.is_empty() && matches!(collection, "agents" | "missions" | "work") {
            return Err(format!("the generated {collection} snapshot is empty"));
        }
        let item_order = frame["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        if item_order.iter().copied().ne(order.iter().map(String::as_str)) {
            return Err("snapshot rows disagree with their order".into());
        }
    } else {
        let removes = <Vec<String> as serde::Deserialize>::deserialize(&frame["removes"])
            .map_err(|error| format!("invalid {collection} removals: {error}"))?;
        for removed in removes {
            if !rows.remove(&removed) {
                return Err(format!("{collection} removed a row outside its previous window"));
            }
        }
        rows.extend(window_row_ids(collection, &frame["upserts"])?);
    }
    if *rows != ordered {
        return Err(format!("{collection} rows disagree with their reconstructed window"));
    }
    Ok(snapshot.store_index)
}

fn roster_matches_oracle(frame: &Value, oracle: &Value) -> Result<(), String> {
    let expected = roster_row_ids(&oracle["items"])?;
    let actual = roster_row_ids(&frame["items"])?;
    if expected.is_empty()
        || expected != actual
        || oracle["page"]["has_more"].as_bool().is_none()
        || frame["has_more"] != oracle["page"]["has_more"]
    {
        return Err("snapshot membership/pagination differs from the HTTP roster".into());
    }
    for (row, reference) in frame["items"]
        .as_array()
        .unwrap()
        .iter()
        .zip(oracle["items"].as_array().unwrap())
    {
        for field in ["kind", "id", "name", "host_id", "workspace"] {
            if row[field] != reference[field] {
                return Err(format!("snapshot {field}/order differs from the HTTP roster"));
            }
        }
    }
    Ok(())
}

#[test]
fn roster_frames_require_correct_cards_membership_and_snapshot_before_changes() {
    let card = json!({
        "kind": "agent", "id": "agent/bench/seat-0", "name": "seat-0",
        "revision": "revision/load", "updated_at": "2026-10-01T00:00:00Z",
        "state": "running", "reachability": "reachable", "host_id": "host/bench",
        "last_activity_at": null, "silent_since": null, "workspace": "/work/bench"
    });
    let frame = json!({
        "kind": "snapshot", "id": "load-roster-0", "collection": "agents",
        "snapshot": {
            "id": "snapshot/load", "host_id": "host/bench", "store_index": 1,
            "projection_version": "client-projection.v0", "created_at": "2026-10-01T00:00:00Z"
        },
        "items": [card], "order": ["agent/bench/seat-0"], "has_more": false
    });
    let oracle = json!({"items": frame["items"], "page": {"has_more": false}});
    let mut rows = BTreeSet::new();
    assert_eq!(apply_roster_frame(&frame, "load-roster-0", true, &mut rows).unwrap(), 1);
    assert!(roster_matches_oracle(&frame, &oracle).is_ok());
    assert!(apply_roster_frame(&frame, "load-roster-0", false, &mut rows).is_err());
    for (field, value) in [
        ("kind", json!("resync")),
        ("id", json!("another-subscription")),
        ("collection", json!("missions")),
        ("items", json!([])),
        ("order", json!([])),
    ] {
        let mut invalid = frame.clone();
        invalid[field] = value;
        assert!(apply_roster_frame(&invalid, "load-roster-0", true, &mut BTreeSet::new()).is_err());
    }
    let mut invalid = frame.clone();
    invalid["items"][0]["kind"] = json!("work");
    assert!(apply_roster_frame(&invalid, "load-roster-0", true, &mut BTreeSet::new()).is_err());
    let mut renamed = frame.clone();
    renamed["items"][0]["name"] = json!("wrong seat");
    assert!(roster_matches_oracle(&renamed, &oracle).is_err());
    let mut change = json!({
        "kind": "changes", "id": "load-roster-0", "collection": "agents",
        "snapshot": frame["snapshot"], "upserts": [], "removes": ["agent/bench/seat-0"],
        "order": [], "has_more": false
    });
    assert!(apply_roster_frame(&change, "load-roster-0", false, &mut rows).is_ok());
    assert!(rows.is_empty());
    change["removes"] = json!(["agent/bench/not-in-window"]);
    assert!(apply_roster_frame(&change, "load-roster-0", false, &mut rows).is_err());
}

#[test]
fn held_windows_require_typed_rows_and_their_own_collection() {
    let frame = |collection: &str, items: Value| {
        let order = items.as_array().unwrap().iter().map(|item| item["id"].clone()).collect::<Vec<_>>();
        json!({
            "kind": "snapshot", "id": "load-x-0", "collection": collection,
            "snapshot": {
                "id": "snapshot/load", "host_id": "host/bench", "store_index": 3,
                "projection_version": "client-projection.v0", "created_at": "2026-10-01T00:00:00Z"
            },
            "items": items, "order": order, "has_more": false
        })
    };
    // Optional windows may start empty; the generated store always has missions and work.
    assert_eq!(apply_window_frame("glasses", &frame("glasses", json!([])), "load-x-0", true, &mut BTreeSet::new()), Ok(3));
    assert!(apply_window_frame("missions", &frame("missions", json!([])), "load-x-0", true, &mut BTreeSet::new()).is_err());
    assert!(apply_window_frame("work", &frame("missions", json!([])), "load-x-0", true, &mut BTreeSet::new()).is_err());
    // A row that is not a mission card is not a mission snapshot.
    let untyped = frame("missions", json!([{"id": "mission/bench/one", "kind": "mission"}]));
    assert!(apply_window_frame("missions", &untyped, "load-x-0", true, &mut BTreeSet::new()).is_err());
    let summary = frame("summary", json!([{"id": "summary/current", "kind": "summary"}]));
    let mut rows = BTreeSet::new();
    assert_eq!(apply_window_frame("summary", &summary, "load-x-0", true, &mut rows), Ok(3));
    assert_eq!(rows, BTreeSet::from(["summary/current".to_owned()]));
}

/// One request of the kind `name`, and the name to record it under when that is more exact.
async fn send(context: &Context, name: &str) -> Result<Option<String>, String> {
    send_one(context, name)
        .await
        .map(|()| None)
        .or_else(|error| {
            if error.starts_with("person read ") {
                // A person read that answered names its route.
                Ok(Some(error))
            } else {
                Err(error)
            }
        })
}

async fn send_one(context: &Context, name: &str) -> Result<(), String> {
    let turn = context.turns.fetch_add(1, Ordering::Relaxed);
    let seats = &context.subjects.seats;
    let seat = &seats[turn % seats.len()];
    let encoded = urlencoding::encode(seat).into_owned();
    let client = &context.client;
    let get = |path: String| async move {
        client
            .get::<Value>(&path)
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    };
    let post = |path: &'static str, body: Value| async move {
        client
            .post::<_, Value>(path, &body)
            .await
            .map(drop)
            .map_err(|error| error.to_string())
    };
    let key = format!("load-{name}-{turn}-{}", std::process::id()).replace(' ', "-");
    match name {
        // A native driver's observations go straight to the store: the route binds its caller to
        // a live seat process, which a test has none of.
        "harness event" => {
            let kind = ["harness.timeline", "harness.usage", "harness.observed"][turn % 3];
            let mut claim = claim_input(kind, &key, turn, "");
            claim.subject = seat.clone();
            claim.actor = Some(seat.clone());
            if kind == "harness.timeline" {
                claim
                    .fields
                    .insert("sequence".into(), json!(turn as u64 + 1));
            }
            claim
                .fields
                .insert("incarnation_id".into(), json!(runtime_of(seat)));
            let publication = st3::harness_events::Publication {
                runtime_incarnation: runtime_of(seat),
                sequence: turn as u64 + 1,
                claim,
            };
            let current = kind == "harness.observed";
            let result = on_daemon(context, move |store| {
                Ok(if current {
                    store.append_claim(&publication.claim).map(drop)
                } else {
                    store.append_harness_event(&publication).map(drop)
                })
            })
            .await?;
            match result {
                Err(error)
                    if current
                        && (matches!(
                            error.code,
                            "current-value-deadline" | "database-busy" | "database-locked"
                        ) || error.code == "internal"
                            && (error.message.contains("database is locked")
                                || error.message.contains("database table is locked"))) =>
                {
                    context.current_dropped.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                result => result.map_err(|error| error.message),
            }
        }
        "seat mailbox page" => {
            get(format!(
                "/v1/messages/page?include_closed=false&limit=100&to={encoded}"
            ))
            .await
        }
        // Drivers post what they observe of their runtimes and terminals.
        "claim" => {
            let claim = claim_input("runtime.observed", &key, turn, "");
            post("/v1/claims", serde_json::to_value(claim).unwrap()).await
        }
        // A seat's statusline reads its declared agent's desired state.
        "seat desired state" => {
            let agents = &context.subjects.agents;
            let agent = urlencoding::encode(&agents[turn % agents.len()]).into_owned();
            get(format!("/v1/desired/{agent}")).await
        }
        "delivery hold" => get(format!("/v1/delivery/hold?subject={encoded}")).await,
        "fleet membership" => get("/v1/internal/fleet/membership".into()).await,
        "replication exchange" => replication_exchange(context, turn, &key).await,
        "replication peer failure" => {
            post(
                "/v1/internal/replication/peer-failure",
                json!({"peer": PEER, "status": "down", "error": "an invented timeout"}),
            )
            .await
        }
        "seat status" => get(format!("/v1/status?subject={encoded}")).await,
        "seat work" => get(format!("/v1/work?actor={encoded}")).await,
        "client usage" => get("/v1/client/usage".into()).await,
        "lease renewal" => {
            let (agent, step, incarnation) =
                context.subjects.held[turn % context.subjects.held.len()].clone();
            let request = WorkRequest {
                actor: Some(agent),
                incarnation: Some(incarnation),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: key,
            };
            // The route renews only for a live harness incarnation; the renewal is the same write.
            // The reconciler releases a lease whose seat has no live harness, as it should, and
            // the seat claims its step again.
            on_daemon(context, move |store| {
                store
                    .work_action(&step, "renew", &request)
                    .or_else(|error| {
                        if error.message.contains("active lease") {
                            store.work_action(&step, "claim", &request)
                        } else {
                            Err(error)
                        }
                    })
                    .map(drop)
                    .map_err(|error| error.message)
            })
            .await
        }
        "replication wake" => post("/v1/internal/replication-wake", json!({})).await,
        "message send" => {
            post(
                "/v1/messages",
                json!({
                    "idempotency_key": key,
                    "from": seat,
                    "to": seats[(turn + 1) % seats.len()],
                    "title": "An invented status note",
                    "content": "The invented build finished; the invented review can start.",
                }),
            )
            .await
        }
        "person read" => {
            let (path, _) = PERSON_READS[turn % PERSON_READS.len()];
            match context.person.get::<Value>(path).await {
                // Recorded per route: `send` turns this into the route's name.
                Ok(_) => Err(format!(
                    "person read {}",
                    path.split('?').next().unwrap_or(path)
                )),
                Err(error) => Err(format!("{path}: {error}")),
            }
        }
        other => Err(format!("no request named {other}")),
    }
}

/// The running runtime of `seat`, whose driver publishes its harness events.
fn runtime_of(seat: &str) -> String {
    format!("load-{}-runtime", seat.rsplit('/').next().unwrap_or(seat))
}

/// A request's budget: its kind's, or for a person read, its route's.
fn budget(budgets: &BTreeMap<&str, Duration>, name: &str) -> Duration {
    // A successful quiet poll intentionally waits 30 seconds before answering.
    if name == LONG_POLL {
        return Duration::from_secs(31);
    }
    if matches!(name, ROSTER_SNAPSHOT | ROSTER_CONNECT_SNAPSHOT)
        || COLLECTION_WINDOWS
            .iter()
            .any(|collection| snapshot_names(collection).iter().any(|snapshot| snapshot == name))
    {
        return ROSTER_BUDGET;
    }
    if let Some(route) = name.strip_prefix("person read ") {
        let budget = PERSON_READS
            .iter()
            .find(|(path, _)| path.split('?').next() == Some(route))
            .map_or(250, |(_, budget)| *budget);
        return Duration::from_millis(budget);
    }
    budgets
        .get(name)
        .copied()
        .unwrap_or(Duration::from_millis(250))
}

/// Run `work` on the daemon's blocking pool, where its CPU counts as the daemon's.
async fn on_daemon<T: Send + 'static>(
    context: &Context,
    work: impl FnOnce(&Store) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let store = context.store.clone();
    context
        .daemon
        .spawn_blocking(move || work(&store))
        .await
        .map_err(|error| error.to_string())?
}

/// One exchange as the replication worker runs it: the daemon exports a summary and an exchange
/// answering the peer's, and receives the peer's new claims.
async fn replication_exchange(context: &Context, turn: usize, key: &str) -> Result<(), String> {
    let client = &context.client;
    let summary = client
        .post::<_, Value>(
            "/v1/internal/replication/export",
            &json!({"fleet_id": FLEET, "inventory": {}, "summary_only": true}),
        )
        .await
        .map_err(|error| error.to_string())?;
    let peer = context.peer.clone();
    let key = key.to_owned();
    // The peer's side runs on the load runtime: on a real fleet it is another machine.
    let (peer_inventory, exchange) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        // Current observations are outside graph inventories. Exercise durable replication
        // with a fresh message rather than an empty register-only exchange.
        let mut input = claim_input("message.sent", &key, turn, "");
        input.subject = format!("message/{key}");
        peer.append_claim(&input)?;
        let inventory =
            serde_json::from_value(summary["exchange"]["inventory"].clone()).unwrap_or_default();
        let exchange = peer.export_replication_exchange(FLEET, &inventory)?;
        Ok((peer.export_replication_summary(FLEET)?.inventory, exchange))
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    client
        .post::<_, Value>(
            "/v1/internal/replication/receive",
            &json!({"peer": PEER, "fleet_id": FLEET, "exchange": exchange, "round_trip_ms": 5}),
        )
        .await
        .map_err(|error| error.to_string())?;
    let answer = client
        .post::<_, Value>(
            "/v1/internal/replication/export",
            &json!({"fleet_id": FLEET, "inventory": peer_inventory, "summary_only": false}),
        )
        .await
        .map_err(|error| error.to_string())?;
    let peer = context.peer.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let exchange = serde_json::from_value(answer["exchange"].clone())?;
        peer.receive_replication_exchange(NODE, FLEET, &exchange)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        peer.validate_replication_backlog()?;
        peer.apply_replication_repairs()?;
        peer.project_replication_backlog()?;
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// This process's threads.
fn threads() -> BTreeSet<u32> {
    std::fs::read_dir("/proc/self/task")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// CPU seconds the whole process used, finished threads included.
fn process_cpu() -> f64 {
    // SAFETY: getrusage fills the zeroed struct.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// CPU seconds the load's threads used: the load runtime's and the peer store's.
fn load_cpu(peer_threads: &BTreeSet<u32>) -> f64 {
    // SAFETY: sysconf has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    threads()
        .into_iter()
        .filter_map(|thread| {
            let stat = std::fs::read_to_string(format!("/proc/self/task/{thread}/stat")).ok()?;
            // The name sits in parentheses and may hold spaces; the fields after it are fixed.
            let (name, fields) = stat.split_once('(')?.1.rsplit_once(')')?;
            let load = name.starts_with("st3-load") || peer_threads.contains(&thread);
            let fields = fields.split_whitespace().collect::<Vec<_>>();
            // utime and stime are the 14th and 15th fields; the state is the 3rd.
            let cpu = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
            load.then_some(cpu / ticks)
        })
        .sum()
}
