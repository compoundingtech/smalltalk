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
//! and a person moving through stui. Only kinds and rates; no contents.
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
//! - `ST_LOAD_REPORT` writes this run's report there, for the next comparison.

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
use tokio::sync::{Notify, watch};

use crate::daemon_bench::{
    FLEET, NODE, PEER, Subjects, claim_input, env_number, fleet_subjects, generated_stores,
    percentile, stub_pty,
};

/// How much worse than main's baseline a p99 or the daemon's CPU may be.
const WORSE: f64 = 1.2;

/// A p99 this close to the baseline passes whatever the ratio: a few milliseconds of noise.
const LATENCY_SLACK: Duration = Duration::from_millis(5);

/// The daemon's CPU, in cores, may differ from the baseline by this much whatever the ratio.
const CPU_SLACK: f64 = 0.05;

/// The daemon's average CPU over the run may not pass this many cores.
const CPU_BUDGET: f64 = 2.0;

/// Requests in flight at once before the load stops adding more; a daemon this far behind fails.
const IN_FLIGHT_LIMIT: usize = 256;

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
    scale: f64,
    claims: u64,
    seconds: f64,
    /// The daemon's average CPU over the run, in cores.
    daemon_cores: f64,
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
    let scale = env_number("ST_LOAD_SCALE", 1.0_f64);
    let seconds = env_number("ST_LOAD_SECONDS", 120_u64);
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();

    let daemon = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("st3-daemon")
        .enable_all()
        .build()
        .unwrap();
    let started = Instant::now();
    let (source, peer_source) = daemon.block_on(generated_stores(&keep, scale));
    println!("store ready in {:.0}s", started.elapsed().as_secs_f64());
    let report = run(
        &daemon,
        scale,
        &source,
        &peer_source,
        Duration::from_secs(seconds),
    );
    print(&report);
    if let Some(path) = std::env::var_os("ST_LOAD_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }

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
    if report.daemon_cores > CPU_BUDGET {
        failures.push(format!(
            "the daemon used {:.2} cores, over its {CPU_BUDGET} budget",
            report.daemon_cores
        ));
    }
    let requests = report.paths.values().map(|path| path.count).sum::<usize>();
    let failed = report.failed.values().sum::<usize>();
    if failed * 100 > requests {
        failures.push(format!(
            "{failed} of {requests} requests failed: {:?}",
            report.failed
        ));
    }
    if let Some(baseline) = std::env::var_os("ST_LOAD_BASELINE") {
        match worst_of(Path::new(&baseline)) {
            Some(baseline) => failures.extend(compare(&report, &baseline)),
            None => println!(
                "no baseline at {}; only the budgets apply",
                Path::new(&baseline).display()
            ),
        }
    }
    assert!(
        failures.is_empty(),
        "the daemon missed {} budgets:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Main's reports at `path`, a report or a directory of them, combined into the worst of each:
/// one run's p99 on a shared runner moves by half or more from the next's, so a regression is
/// what passes the worst of several runs.
fn worst_of(path: &Path) -> Option<Report> {
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
    (!reports.is_empty()).then_some(worst)
}

/// Where this run is more than [`WORSE`] past the baseline. A path with few requests has a p99
/// that is nearly its maximum, which one slow request moves, so only busy paths compare.
fn compare(report: &Report, baseline: &Report) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, path) in &report.paths {
        let Some(before) = baseline.paths.get(name) else {
            continue;
        };
        if path.count < 100 || before.count < 100 {
            continue;
        }
        if path.p99_ms > before.p99_ms * WORSE
            && path.p99_ms > before.p99_ms + LATENCY_SLACK.as_secs_f64() * 1e3
        {
            failures.push(format!(
                "{name}: p99 {:.1} ms is more than {:.0}% over main's {:.1} ms",
                path.p99_ms,
                (WORSE - 1.0) * 100.0,
                before.p99_ms
            ));
        }
    }
    if report.daemon_cores > baseline.daemon_cores * WORSE
        && report.daemon_cores > baseline.daemon_cores + CPU_SLACK
    {
        failures.push(format!(
            "the daemon used {:.2} cores, more than {:.0}% over main's {:.2}",
            report.daemon_cores,
            (WORSE - 1.0) * 100.0,
            baseline.daemon_cores
        ));
    }
    failures
}

fn print(report: &Report) {
    println!(
        "\n== load test: scale {}, {} claims, {:.0}s, daemon {:.2} cores",
        report.scale, report.claims, report.seconds, report.daemon_cores
    );
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
}

fn run(
    daemon: &tokio::runtime::Runtime,
    scale: f64,
    source: &Path,
    peer_source: &Path,
    duration: Duration,
) -> Report {
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

    let store = Arc::new(Store::open(&database, NODE).unwrap());
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
        let server_socket = socket.clone();
        daemon.spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
        let subjects = fleet_subjects(&store, 30);
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
    });
    // Let the daemon settle after opening: its first reconciler pass is not the load's.
    std::thread::sleep(Duration::from_secs(5));

    let timings = Arc::new(Mutex::new(BTreeMap::<String, Vec<Duration>>::new()));
    let failed = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let cpu_before = (process_cpu(), load_cpu(&peer_threads));
    let started = Instant::now();
    load.block_on(async {
        let mut tasks = Vec::new();
        for (index, kind) in MIX.iter().enumerate() {
            let (context, timings, failed, in_flight, running) = (
                context.clone(),
                timings.clone(),
                failed.clone(),
                in_flight.clone(),
                running.clone(),
            );
            tasks.push(tokio::spawn(async move {
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
        tokio::time::sleep(duration).await;
        running.store(false, Ordering::Relaxed);
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
    let daemon_cpu = (cpu_after.0 - cpu_before.0) - (cpu_after.1 - cpu_before.1);

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
    Report {
        scale,
        claims,
        seconds: elapsed,
        daemon_cores: daemon_cpu / elapsed,
        paths,
        failed,
    }
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
            on_daemon(context, move |store| {
                store
                    .append_harness_event(&publication)
                    .map(drop)
                    .map_err(|error| error.message)
            })
            .await
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
        peer.append_claim(&claim_input("harness.observed", &key, turn, ""))?;
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
