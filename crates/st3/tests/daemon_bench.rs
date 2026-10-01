//! How fast the daemon answers the reads people make, on a store the size of a busy fleet host's
//! and larger, while seats poll and write and a peer replicates at the rates measured on that
//! host.
//!
//! ```sh
//! ST_BENCH=1 TMPDIR=/var/tmp cargo test --release -p st3 --test integration daemon_bench:: -- --nocapture
//! ```
//!
//! A generated store has the mix of a busy host's store on 2026-09-29, by count only: 270,000
//! claims of about 100 kinds, 355 missions, 1,545 runs, 4,700 steps, 5,000 messages, documents,
//! standing agents and a peer's replicated history, all with invented names. Scale `1` is that
//! store; `10` is ten times each count. It leaves out the sampled store's remembered observer
//! polls (200,000 small rows found by primary key), since its observers poll real providers;
//! a copy of a real store has them.
//!
//! - `ST_BENCH=1` runs it. A debug build skips it.
//! - `ST_BENCH_SCALES` lists the generated scales, smallest first. The default is `0.1,1`.
//! - `ST_BENCH_STORE` also benchmarks a copy of that store, for example a person's own. The copy
//!   stays in `TMPDIR` and only timings and counts are printed, never contents.
//! - `ST_BENCH_DIR` keeps generated stores for the next run. The default, `target/st-bench`, is
//!   ignored by git. A store at scale 10 takes about half an hour to generate and 20 GB.
//! - `ST_BENCH_SECONDS` sets how long the fleet runs against each store. The default is 60.
//! - `ST_BENCH_SEATS` sets how many seats poll and write. The default is 30.
//! - `ST_BENCH_PEOPLE` sets how many people read at once. The default is 3.
//! - `ST_BENCH_P99_MS` sets the read budget. The default is 200.
//! - `ST3_PROFILE_DIR` profiles the daemon during the runs, as `docs/st3/profiling.md` describes.
//! - `ST_BENCH_RECONCILER=0` leaves out the reconciler, which otherwise runs as a host that owns
//!   none of the store's members, so it starts, renders and signals nothing.
//!
//! It fails when a person-facing read's p99 passes the budget, and when a read's median grows by
//! more than twice from one scale to the next, which means it does work that grows with the store.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use st3::api::AppState;
use st3::client::Client;
use st3::model::{
    ClaimInput, IntentInput, MissionRunRequest, PersonAskRequest, PersonStepResponse, WorkRequest,
};
use st3::store::Store;
use st3_schema::ValueType;
use tokio::sync::{Notify, watch};

const FLEET: &str = "7c1e2d3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f";
/// The benchmarked daemon's node. No member of a generated or copied store runs here.
const NODE: &str = "bench-host";
/// The peer whose history the generated store replicated, and that replicates during the run.
const PEER: &str = "bench-peer";

/// The reads a person makes from the CLI, stui and the app, and the ones every seat makes.
/// `{agent}`, `{mission}`, `{step}` and `{message}` stand for items the list reads return, and
/// `{seat}` for a seat of the synthetic fleet.
const READS: &[(&str, &str)] = &[
    ("now", "/v1/client/now"),
    ("agents", "/v1/client/agents"),
    ("agent", "/v1/client/agents/{agent}"),
    ("missions", "/v1/client/missions"),
    ("missions tree", "/v1/client/missions-tree"),
    ("mission", "/v1/client/missions/{mission}"),
    ("machines", "/v1/client/machines"),
    ("attention", "/v1/client/attention"),
    ("messages", "/v1/client/messages"),
    ("message read", "/v1/messages/read/{message}"),
    ("work", "/v1/client/work"),
    ("work item", "/v1/client/work/{step}"),
    ("history", "/v1/client/history"),
    ("sessions", "/v1/client/sessions"),
    ("runtimes", "/v1/client/runtimes"),
    ("terminals", "/v1/client/terminals"),
    ("lanes", "/v1/client/lanes"),
    ("events", "/v1/client/events"),
    ("observers", "/v1/client/observers"),
    ("subscriptions", "/v1/client/subscriptions"),
    ("operations", "/v1/client/operations"),
    ("status of a seat", "/v1/status?subject={seat}"),
    // No client or command reads every subject's status at once; it is here to show its cost.
    ("status of every subject", "/v1/status"),
    (
        "seat mailbox",
        "/v1/messages/page?include_closed=false&limit=100&to={seat}",
    ),
    // A seat reads its own work to renew its leases each minute.
    ("seat work", "/v1/work?actor={seat}"),
    ("every seat's work", "/v1/work"),
    ("mission runs", "/v1/mission-runs?mission={mission}"),
    ("attention list", "/v1/attention"),
    ("replication status", "/v1/replication/status"),
    ("usage", "/v1/usage"),
];

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn person_facing_reads_answer_within_their_budget() {
    if std::env::var_os("ST_BENCH").is_none() {
        println!("skipped: set ST_BENCH=1 to run the daemon benchmark");
        return;
    }
    if cfg!(debug_assertions) {
        println!("skipped: a debug build is too slow to measure; run with cargo test --release");
        return;
    }
    // `ST3_PROFILE_DIR` profiles the daemon here as it does under `st up`.
    st3::profile::init_from_env();
    let scales = std::env::var("ST_BENCH_SCALES")
        .unwrap_or_else(|_| "0.1,1".into())
        .split(',')
        .map(|scale| {
            scale
                .trim()
                .parse::<f64>()
                .expect("ST_BENCH_SCALES lists numbers")
        })
        .collect::<Vec<_>>();
    let settings = Settings {
        reads: READS,
        seconds: env_number("ST_BENCH_SECONDS", 60),
        seats: env_number("ST_BENCH_SEATS", 30),
        people: env_number("ST_BENCH_PEOPLE", 3),
        budget: Duration::from_millis(env_number("ST_BENCH_P99_MS", 200)),
        reconciler: std::env::var("ST_BENCH_RECONCILER").map_or(true, |value| value != "0"),
    };
    let keep = std::env::var_os("ST_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/st-bench"));
    std::fs::create_dir_all(&keep).unwrap();

    let mut runs = Vec::new();
    for scale in &scales {
        let store = generated_store(&keep, *scale).await;
        runs.push(bench(&format!("scale {scale}"), &store, &settings).await);
    }
    if let Some(private) = std::env::var_os("ST_BENCH_STORE") {
        runs.push(bench("private store", Path::new(&private), &settings).await);
    }

    let mut failures = Vec::new();
    for run in &runs {
        run.print();
        for (name, samples) in &run.reads {
            let p99 = percentile(samples, 99);
            if p99 > settings.budget {
                failures.push(format!(
                    "{}: {name} p99 {} ms is over {} ms",
                    run.name,
                    p99.as_millis(),
                    settings.budget.as_millis()
                ));
            }
        }
    }
    for pair in runs
        .windows(2)
        .filter(|pair| pair[1].claims > pair[0].claims * 3)
    {
        for (name, samples) in &pair[1].reads {
            let Some(smaller) = pair[0].reads.get(name) else {
                continue;
            };
            let (before, after) = (percentile(smaller, 50), percentile(samples, 50));
            if after > before * 2 && after > before + Duration::from_millis(20) {
                failures.push(format!(
                    "{name} grows with the store: median {} ms at {} claims, {} ms at {} claims",
                    before.as_millis(),
                    pair[0].claims,
                    after.as_millis(),
                    pair[1].claims
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} reads miss their budget:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The reads a person makes of status, agents, missions, attention, messages and attach, which
/// must answer within their budget however busy the fleet is.
const PERSON_READS: &[(&str, &str)] = &[
    ("status of a seat", "/v1/status?subject={seat}"),
    ("now", "/v1/client/now"),
    ("agents", "/v1/client/agents"),
    ("agent", "/v1/client/agents/{agent}"),
    ("missions", "/v1/client/missions"),
    ("mission", "/v1/client/missions/{mission}"),
    ("attention", "/v1/client/attention"),
    ("attention list", "/v1/attention"),
    (
        "seat mailbox",
        "/v1/messages/page?include_closed=false&limit=100&to={seat}",
    ),
    ("messages", "/v1/client/messages"),
    ("message read", "/v1/messages/read/{message}"),
    ("terminals", "/v1/client/terminals"),
];

/// The load test that runs with every other test: thirty seats and a busy writer against a
/// small generated store, while a person reads status, agents, missions, attention, messages
/// and terminals. Each read's p99 must stay under 200 ms (`ST_LOAD_P99_MS`).
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn person_reads_stay_within_budget_while_thirty_seats_and_a_writer_work() {
    if std::env::var_os("NIX_BUILD_TOP").is_some() {
        println!("skipped: a Nix build sandbox is no place to time reads");
        return;
    }
    let keep = tempfile::tempdir().unwrap();
    let store = generated_store(keep.path(), 0.01).await;
    let settings = Settings {
        reads: PERSON_READS,
        seconds: env_number("ST_LOAD_SECONDS", 20),
        seats: 30,
        people: 1,
        budget: Duration::from_millis(env_number("ST_LOAD_P99_MS", 200)),
        reconciler: true,
    };
    let run = bench("load test", &store, &settings).await;
    run.print();
    let over = run
        .reads
        .iter()
        .map(|(read, samples)| (read, percentile(samples, 99)))
        .filter(|(_, p99)| *p99 > settings.budget)
        .map(|(read, p99)| format!("{read} p99 {} ms", p99.as_millis()))
        .collect::<Vec<_>>();
    assert!(
        over.is_empty(),
        "reads over {} ms: {}",
        settings.budget.as_millis(),
        over.join(", ")
    );
    let unanswered = PERSON_READS
        .iter()
        .filter(|(read, _)| run.reads.get(*read).is_none_or(Vec::is_empty))
        .map(|(read, _)| *read)
        .collect::<Vec<_>>();
    assert!(
        unanswered.is_empty(),
        "reads that never answered: {unanswered:?}; failures {:?}",
        run.failed
    );
}

/// A burst of writes, as when every seat reconnects after a restart: each seat writes a
/// heartbeat at the same moment, twenty times over. Prints each write's latency and how many
/// commits, each a disk flush, the store made for them. Run it in a release build with
/// `ST_BENCH=1`; `TMPDIR` picks the disk.
#[test]
fn write_bursts_share_their_commits() {
    if std::env::var_os("ST_BENCH").is_none() {
        println!("skipped: set ST_BENCH=1 to run the daemon benchmark");
        return;
    }
    if cfg!(debug_assertions) {
        println!("skipped: a debug build is too slow to measure; run with cargo test --release");
        return;
    }
    let seats = env_number("ST_BENCH_SEATS", 30);
    let rounds = 20;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&directory.path().join("claims.sqlite3"), NODE).unwrap());
    let commits_before = store.replication_timings().commits;
    let started = Instant::now();
    let mut latencies = Vec::new();
    for round in 0..rounds {
        let start = Arc::new(std::sync::Barrier::new(seats));
        let writers = (0..seats)
            .map(|seat| {
                let (store, start) = (store.clone(), start.clone());
                std::thread::spawn(move || {
                    let index = round * seats + seat;
                    let input =
                        claim_input("harness.observed", &format!("burst-{index}"), index, "");
                    start.wait();
                    let started = Instant::now();
                    store.append_claim(&input).unwrap();
                    started.elapsed()
                })
            })
            .collect::<Vec<_>>();
        latencies.extend(writers.into_iter().map(|writer| writer.join().unwrap()));
    }
    let elapsed = started.elapsed();
    let commits = store.replication_timings().commits - commits_before;
    println!(
        "write bursts: {seats} seats x {rounds} rounds, {} writes in {} ms, {commits} commits; \
         p50 {} ms, p90 {} ms, p99 {} ms, max {} ms",
        latencies.len(),
        elapsed.as_millis(),
        percentile(&latencies, 50).as_millis(),
        percentile(&latencies, 90).as_millis(),
        percentile(&latencies, 99).as_millis(),
        latencies.iter().max().unwrap().as_millis(),
    );
}

struct Settings {
    /// The reads the people make, in turn.
    reads: &'static [(&'static str, &'static str)],
    seconds: u64,
    seats: usize,
    people: usize,
    budget: Duration,
    reconciler: bool,
}

/// What one store measured.
struct Run {
    name: String,
    claims: u64,
    bytes: u64,
    reads: BTreeMap<String, Vec<Duration>>,
    writes: BTreeMap<String, Vec<Duration>>,
    failed: BTreeMap<String, usize>,
}

impl Run {
    fn print(&self) {
        println!(
            "\n== {}: {} claims, {:.1} GB",
            self.name,
            self.claims,
            self.bytes as f64 / 1e9
        );
        println!(
            "{:<22} {:>6} {:>8} {:>8} {:>8} {:>8}",
            "read", "n", "p50 ms", "p90 ms", "p99 ms", "max ms"
        );
        let row = |name: &str, samples: &Vec<Duration>| {
            println!(
                "{:<22} {:>6} {:>8} {:>8} {:>8} {:>8}",
                name,
                samples.len(),
                percentile(samples, 50).as_millis(),
                percentile(samples, 90).as_millis(),
                percentile(samples, 99).as_millis(),
                samples
                    .iter()
                    .max()
                    .copied()
                    .unwrap_or_default()
                    .as_millis()
            );
        };
        for (name, samples) in &self.reads {
            row(name, samples);
        }
        println!("{:<22}", "write");
        for (name, samples) in &self.writes {
            row(name, samples);
        }
        if !self.failed.is_empty() {
            println!("failed requests: {:?}", self.failed);
        }
    }
}

fn percentile(samples: &[Duration], percent: usize) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (sorted.len() * percent).div_ceil(100).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------------------------
// The benchmark: one store, a synthetic fleet, and timed reads.
// ---------------------------------------------------------------------------------------------

/// Everything the fleet and the probes refer to.
#[derive(Clone, Default)]
struct Subjects {
    /// The synthetic fleet's seats.
    seats: Vec<String>,
    agents: Vec<String>,
    missions: Vec<String>,
    steps: Vec<String>,
    runs: Vec<String>,
    messages: Vec<String>,
    /// One claimed step per seat, which that seat renews.
    held: Vec<(String, String, String)>,
}

async fn bench(name: &str, source: &Path, settings: &Settings) -> Run {
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    println!("\n{name}: copying the store");
    let database = root.join("state/claims.sqlite3");
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    for suffix in ["", "-wal"] {
        let from = PathBuf::from(format!("{}{suffix}", source.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", database.display())).unwrap();
        }
    }
    let opened = Instant::now();
    let store = Arc::new(Store::open(&database, NODE).unwrap());
    store.bind_fleet(FLEET).ok();
    println!("{name}: opened in {:.1}s", opened.elapsed().as_secs_f64());
    let claims = store.index().unwrap();
    let bytes = std::fs::metadata(&database)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

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
    // As the daemon does when it starts.
    st3::api::start_operation_report(&state);
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let subjects = {
        let store = store.clone();
        let seats = settings.seats;
        tokio::task::spawn_blocking(move || fleet_subjects(&store, seats))
            .await
            .unwrap()
    };
    // The fleet takes its steps before the reconciler starts, which could otherwise block a step
    // between its turning ready and its claim.
    let reconciler = settings.reconciler.then(|| {
        let reconciler = Arc::new(
            st3::reconcile::Reconciler::native(
                store.clone(),
                &root.join("state"),
                Some(&root.join("pty")),
                &pty,
                NODE.into(),
                socket.display().to_string(),
                notify.clone(),
                event_notify.clone(),
                None,
            )
            .unwrap(),
        );
        tokio::spawn(reconciler.supervise())
    });

    let client = Client::unix(&socket);
    let mut subjects = subjects;
    subjects.agents = listed(&client, "/v1/client/agents").await;
    subjects.missions = listed(&client, "/v1/client/missions").await;
    subjects.steps = listed(&client, "/v1/client/work").await;
    subjects.messages = listed(&client, "/v1/client/messages").await;
    if subjects.messages.is_empty() {
        let store = store.clone();
        subjects.messages = tokio::task::spawn_blocking(move || {
            store
                .claims_page(None, None, 0, None, true, 5_000)
                .map(|page| {
                    page.claims
                        .into_iter()
                        .filter(|claim| claim.kind == "message.sent")
                        .map(|claim| claim.subject)
                        .take(50)
                        .collect()
                })
                .unwrap_or_default()
        })
        .await
        .unwrap();
    }
    let running = Arc::new(AtomicBool::new(true));
    let writes = Arc::new(Mutex::new(BTreeMap::<String, Vec<Duration>>::new()));
    let failed = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let mut tasks = Vec::new();
    for seat in 0..settings.seats {
        tasks.push(tokio::spawn(seat_loop(
            seat,
            store.clone(),
            client.clone(),
            subjects.clone(),
            running.clone(),
            writes.clone(),
            failed.clone(),
        )));
    }
    tasks.push(tokio::spawn(writer_loop(
        store.clone(),
        client.clone(),
        subjects.clone(),
        running.clone(),
        writes.clone(),
        failed.clone(),
    )));
    tasks.push(tokio::spawn(peer_loop(
        store.clone(),
        root.to_path_buf(),
        running.clone(),
        writes.clone(),
        failed.clone(),
    )));

    // People read one route after another, as someone moving through stui or the CLI does: a few
    // of them at once, each starting at a different route, so a slow route cannot starve the
    // others of samples.
    let deadline = Instant::now() + Duration::from_secs(settings.seconds);
    let mut people = Vec::new();
    for person in 0..settings.people {
        let (client, subjects, failed) = (client.clone(), subjects.clone(), failed.clone());
        let reads = settings.reads;
        people.push(tokio::spawn(async move {
            let mut timings = BTreeMap::<String, Vec<Duration>>::new();
            let mut round = person;
            while Instant::now() < deadline {
                for offset in 0..reads.len() {
                    let (read, path) = reads[(offset + person * reads.len() / 3) % reads.len()];
                    let path = fill(path, &subjects, round);
                    let started = Instant::now();
                    let answer =
                        tokio::time::timeout(Duration::from_secs(60), client.get::<Value>(&path))
                            .await;
                    match answer {
                        Ok(Ok(_)) => timings
                            .entry(read.into())
                            .or_default()
                            .push(started.elapsed()),
                        Ok(Err(error)) => {
                            *failed
                                .lock()
                                .unwrap()
                                .entry(format!("{read}: {}", short(&error)))
                                .or_default() += 1
                        }
                        Err(_) => {
                            *failed
                                .lock()
                                .unwrap()
                                .entry(format!("{read}: timed out"))
                                .or_default() += 1
                        }
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                round += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            timings
        }));
    }
    let mut reads = BTreeMap::<String, Vec<Duration>>::new();
    for person in people {
        for (read, samples) in person.await.unwrap() {
            reads.entry(read).or_default().extend(samples);
        }
    }
    running.store(false, Ordering::Relaxed);
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(30), task).await;
    }
    if let Some(reconciler) = reconciler {
        reconciler.abort();
    }
    server.abort();
    let writes = std::mem::take(&mut *writes.lock().unwrap());
    let failed = std::mem::take(&mut *failed.lock().unwrap());
    Run {
        name: name.into(),
        claims,
        bytes,
        reads,
        writes,
        failed,
    }
}

fn fill(path: &str, subjects: &Subjects, round: usize) -> String {
    let pick = |items: &[String]| {
        items
            .get(round % items.len().max(1))
            .cloned()
            .unwrap_or_default()
    };
    path.replace("{seat}", &urlencoding::encode(&pick(&subjects.seats)))
        .replace("{agent}", &pick(&subjects.agents))
        .replace("{mission}", &pick(&subjects.missions))
        .replace("{step}", &pick(&subjects.steps))
        .replace("{message}", &pick(&subjects.messages))
}

/// The ids of the first page of a client list, for the detail reads.
async fn listed(client: &Client, path: &str) -> Vec<String> {
    let page = client.get::<Value>(path).await.unwrap_or(Value::Null);
    page.get("value")
        .unwrap_or(&page)
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A `pty` that lists no terminals and starts none.
fn stub_pty(root: &Path) -> PathBuf {
    let path = root.join("pty");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = list ]; then echo '[]'; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The subjects the fleet uses: seats with a step each to hold, and runs, steps and messages for
/// the detail reads.
fn fleet_subjects(store: &Store, seats: usize) -> Subjects {
    let mut subjects = Subjects::default();
    let runs = store
        .mission_runs()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .take(50)
        .collect::<Vec<_>>();
    for run in &runs {
        subjects.runs.push(run.subject.clone());
        for step in &run.steps {
            subjects.steps.push(step.subject.clone());
        }
    }
    // Each seat holds a step of its own, in a mission published for the benchmark.
    let mut source = r#"version 2
mission "bench/fleet" state="ready" {
  goal "Keep one step leased per seat."
  concurrent-runs max=1000000
"#
    .to_owned();
    for seat in 0..seats {
        source.push_str(&format!(
            "  step \"hold-{seat}\" {{ assigned-to \"agent/bench/seat-{seat}\" }}\n"
        ));
    }
    source.push_str("}\n");
    let intent = st3::parse_intent(&source, NODE).unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    let _ = store.apply(&intent, &planned.subject_tokens, "bench-fleet-mission");
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "bench/fleet".into(),
            revision: None,
            workspace: "/srv/bench/fleet".into(),
            requester: Some("person/bench-operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("bench-fleet-run-{}", std::process::id()),
        })
        .unwrap();
    for step in &run.steps {
        let seat = step
            .subject
            .rsplit_once("/hold-")
            .and_then(|(_, seat)| seat.parse::<usize>().ok())
            .expect("each fleet step is named for its seat");
        let agent = format!("agent/bench/seat-{seat}");
        store.set_step_state(&step.subject, "ready", None).unwrap();
        let incarnation = format!("seat-{seat}");
        store
            .work_action(
                &step.subject,
                "claim",
                &WorkRequest {
                    actor: Some(agent.clone()),
                    incarnation: Some(incarnation.clone()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("bench-fleet-claim-{seat}-{}", std::process::id()),
                },
            )
            .unwrap();
        subjects
            .held
            .push((agent, step.subject.clone(), incarnation));
    }
    subjects.held.sort_by_key(|(agent, ..)| {
        agent
            .rsplit_once('-')
            .and_then(|(_, seat)| seat.parse::<usize>().ok())
    });
    subjects.seats = (0..seats)
        .map(|seat| format!("agent/bench/seat-{seat}"))
        .collect();
    subjects
}

fn short(error: &anyhow::Error) -> String {
    let text = error.to_string();
    text.chars().take(90).collect()
}

fn record(timings: &Mutex<BTreeMap<String, Vec<Duration>>>, name: &str, took: Duration) {
    timings
        .lock()
        .unwrap()
        .entry(name.into())
        .or_default()
        .push(took);
}

/// One seat, as a harness driver runs it: its mailbox every second, its status and work every
/// minute, a heartbeat every half minute, and a lease renewal every minute.
async fn seat_loop(
    seat: usize,
    store: Arc<Store>,
    client: Client,
    subjects: Subjects,
    running: Arc<AtomicBool>,
    writes: Arc<Mutex<BTreeMap<String, Vec<Duration>>>>,
    failed: Arc<Mutex<BTreeMap<String, usize>>>,
) {
    let agent = format!("agent/bench/seat-{seat}");
    let (holder, step, incarnation) = subjects.held[seat % subjects.held.len()].clone();
    let mailbox = format!(
        "/v1/messages/page?include_closed=false&limit=100&to={}",
        urlencoding::encode(&agent)
    );
    // Spread the seats over the first second and minute, as seats that started at different
    // times do.
    tokio::time::sleep(Duration::from_millis((seat as u64 * 37) % 1000)).await;
    let mut tick = seat * 7;
    while running.load(Ordering::Relaxed) {
        let _ = client.get::<Value>(&mailbox).await;
        if tick.is_multiple_of(30) {
            let started = Instant::now();
            let heartbeat = client
                .post::<_, Value>(
                    "/v1/claims",
                    &json!({
                        "subject": agent,
                        "kind": "harness.observed",
                        "actor": agent,
                        "fields": {"state": if tick % 60 == 0 { "idle" } else { "working" },
                                   "incarnation_id": format!("seat-{seat}")},
                        "evidence": [],
                    }),
                )
                .await;
            match heartbeat {
                Ok(_) => record(&writes, "heartbeat claim", started.elapsed()),
                Err(_) => {
                    *failed
                        .lock()
                        .unwrap()
                        .entry("heartbeat claim".into())
                        .or_default() += 1
                }
            }
        }
        if tick % 60 == 17 {
            let _ = client
                .get::<Value>(&format!(
                    "/v1/status?subject={}",
                    urlencoding::encode(&agent)
                ))
                .await;
            let _ = client.get::<Value>("/v1/work").await;
            // The API renews only for a live harness incarnation, which a synthetic seat has
            // none of; the renewal itself is the same write.
            let started = Instant::now();
            let request = WorkRequest {
                actor: Some(holder.clone()),
                incarnation: Some(incarnation.clone()),
                summary: None,
                reason: None,
                evidence: Vec::new(),
                idempotency_key: format!("bench-renew-{seat}-{tick}-{}", std::process::id()),
            };
            let (store, step) = (store.clone(), step.clone());
            let renewal = tokio::task::spawn_blocking(move || {
                store
                    .work_action(&step, "renew", &request)
                    .map_err(|error| anyhow::anyhow!(error.message))
            })
            .await
            .unwrap();
            match renewal {
                Ok(_) => record(&writes, "lease renewal", started.elapsed()),
                Err(error) => {
                    *failed
                        .lock()
                        .unwrap()
                        .entry(format!("lease renewal: {}", short(&error)))
                        .or_default() += 1
                }
            }
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The rest of the host's writers: the merge train marking its lane, runtime actions, and
/// subscription intake every few seconds, and a message sent and read every ten seconds.
async fn writer_loop(
    store: Arc<Store>,
    client: Client,
    subjects: Subjects,
    running: Arc<AtomicBool>,
    writes: Arc<Mutex<BTreeMap<String, Vec<Duration>>>>,
    failed: Arc<Mutex<BTreeMap<String, usize>>>,
) {
    let mut tick = 0_usize;
    let run = subjects.runs.first().cloned().unwrap_or_default();
    while running.load(Ordering::Relaxed) {
        let kind = [
            "lane.marked",
            "runtime.action.requested",
            "subscription.mission-requested",
        ][tick % 3];
        let input = claim_input(
            kind,
            &format!("bench-writer-{tick}-{}", std::process::id()),
            tick,
            &run,
        );
        let started = Instant::now();
        let store_for_write = store.clone();
        let written = tokio::task::spawn_blocking(move || store_for_write.append_claim(&input))
            .await
            .unwrap();
        match written {
            Ok(_) => record(&writes, "busy writer claim", started.elapsed()),
            Err(_) => {
                *failed
                    .lock()
                    .unwrap()
                    .entry(format!("writer {kind}"))
                    .or_default() += 1
            }
        }
        if tick.is_multiple_of(2) && subjects.seats.len() > 1 {
            let from = &subjects.seats[tick % subjects.seats.len()];
            let to = &subjects.seats[(tick + 1) % subjects.seats.len()];
            let started = Instant::now();
            let sent = client
                .post::<_, Value>(
                    "/v1/messages",
                    &json!({
                        "idempotency_key": format!("bench-message-{tick}-{}", std::process::id()),
                        "from": from,
                        "to": to,
                        "title": "An invented status note",
                        "content": "The invented build finished; the invented review can start.",
                    }),
                )
                .await;
            match sent {
                Ok(message) => {
                    record(&writes, "message send", started.elapsed());
                    if let Some(subject) = message.get("subject").and_then(Value::as_str) {
                        let path = format!(
                            "/v1/messages/{}/claims",
                            urlencoding::encode(subject.trim_start_matches("message/"))
                        );
                        let _ = client
                            .post::<_, Value>(
                                &path,
                                &json!({
                                    "lifecycle": "delivered",
                                    "actor": to,
                                    "evidence": [],
                                    "idempotency_key": format!("bench-delivered-{tick}-{}", std::process::id()),
                                }),
                            )
                            .await;
                        let started = Instant::now();
                        let read = client
                            .post::<_, Value>(
                                &path,
                                &json!({
                                    "lifecycle": "read",
                                    "actor": to,
                                    "evidence": [],
                                    "idempotency_key": format!("bench-read-{tick}-{}", std::process::id()),
                                }),
                            )
                            .await;
                        match read {
                            Ok(_) => record(&writes, "message read receipt", started.elapsed()),
                            Err(error) => {
                                *failed
                                    .lock()
                                    .unwrap()
                                    .entry(format!("message read receipt: {}", short(&error)))
                                    .or_default() += 1
                            }
                        }
                    }
                }
                Err(_) => {
                    *failed
                        .lock()
                        .unwrap()
                        .entry("message send".into())
                        .or_default() += 1
                }
            }
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// A peer that writes run and heartbeat claims and replicates them to the daemon's store every
/// five seconds, as the replication worker delivers a peer's exchanges.
async fn peer_loop(
    store: Arc<Store>,
    root: PathBuf,
    running: Arc<AtomicBool>,
    writes: Arc<Mutex<BTreeMap<String, Vec<Duration>>>>,
    failed: Arc<Mutex<BTreeMap<String, usize>>>,
) {
    let peer = {
        let path = root.join("peer.sqlite3");
        tokio::task::spawn_blocking(move || {
            let peer = Store::open(&path, PEER).unwrap();
            peer.bind_fleet(FLEET).unwrap();
            publish_mission(&peer, "bench/peer-work", 2, 0);
            peer
        })
        .await
        .unwrap()
    };
    let peer = Arc::new(peer);
    let mut tick = 0_usize;
    while running.load(Ordering::Relaxed) {
        let (peer_for_write, store_for_receive) = (peer.clone(), store.clone());
        let started = Instant::now();
        let received = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            // The profiler counts the peer's own writes and the daemon's receipt apart.
            st3::profile::task("bench peer writes", || -> anyhow::Result<()> {
                if tick.is_multiple_of(6) {
                    let run = peer_for_write.create_mission_run(&MissionRunRequest {
                        mission: "bench/peer-work".into(),
                        revision: None,
                        workspace: "/srv/bench/peer".into(),
                        requester: Some("person/bench-operator".into()),
                        mode: Some("run".into()),
                        inputs: BTreeMap::new(),
                        idempotency_key: format!("bench-peer-run-{tick}-{}", std::process::id()),
                    })?;
                    for step in &run.steps {
                        peer_for_write.set_step_state(&step.subject, "ready", None)?;
                    }
                }
                peer_for_write.append_claim(&claim_input(
                    "harness.observed",
                    &format!("bench-peer-heartbeat-{tick}-{}", std::process::id()),
                    tick,
                    "",
                ))?;
                Ok(())
            })?;
            let exchange = peer_for_write
                .export_replication_exchange(FLEET, &store_for_receive.replication_inventory()?)?;
            st3::profile::task("bench replication receive", || -> anyhow::Result<()> {
                store_for_receive
                    .receive_replication_exchange(PEER, FLEET, &exchange)
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                store_for_receive.validate_replication_backlog()?;
                store_for_receive.apply_replication_repairs()?;
                store_for_receive.project_replication_backlog()?;
                Ok(())
            })
        })
        .await
        .unwrap();
        match received {
            Ok(()) => record(&writes, "replication receive", started.elapsed()),
            Err(_) => {
                *failed
                    .lock()
                    .unwrap()
                    .entry("replication receive".into())
                    .or_default() += 1
            }
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The generator: a store with a busy fleet host's mix, from nothing, with invented names.
// ---------------------------------------------------------------------------------------------

/// Claims of each event kind in the sampled store, and how many subjects they were about. Counts
/// only; nothing of the sampled store's content is here.
const EVENT_KINDS: &[(&str, usize, usize)] = &[
    ("subscription.mission-deferred", 38_223, 17),
    ("harness.observed", 33_857, 366),
    ("daemon.diagnostic", 21_751, 3),
    ("harness.timeline", 20_413, 9),
    ("loop.state", 19_987, 157),
    ("observer.observed", 11_474, 89),
    ("runtime.observed", 11_253, 2_545),
    ("harness.usage", 8_228, 103),
    ("transport.observed", 5_713, 3),
    ("runtime.action.succeeded", 5_706, 1_443),
    ("gate.result", 4_275, 4_268),
    ("harness.diagnostic", 3_061, 212),
    ("runtime.action.requested", 2_501, 1_443),
    ("resource.observed", 2_125, 625),
    ("gate.requested", 2_111, 2_111),
    ("subscription.mission-requested", 1_176, 68),
    ("subscription.mission-started", 832, 67),
    ("runtime.reconcile-decision", 742, 11),
    ("observer.state", 679, 93),
    ("render.applied", 526, 336),
    ("daemon.started", 426, 3),
    ("subscription.mission-request-cancelled", 348, 7),
    ("terminal.input.requested", 171, 23),
    ("terminal.input.result", 171, 23),
    ("lane.marked", 166, 1),
    ("loop.round-result", 143, 93),
    ("subscription.state", 107, 107),
    ("publication.operation", 73, 43),
];

/// The sampled store's missions, runs, steps and other objects.
struct Mix {
    missions: usize,
    runs: usize,
    steps_per_run: usize,
    /// Of every hundred steps, how many a seat claims and works.
    worked_percent: usize,
    renewals_per_worked_step: usize,
    messages: usize,
    agents: usize,
    documents: usize,
    attention: usize,
    /// Of every hundred claims, how many a peer wrote and this node replicated.
    peer_percent: usize,
}

const SAMPLED: Mix = Mix {
    missions: 355,
    runs: 1_545,
    steps_per_run: 3,
    worked_percent: 25,
    renewals_per_worked_step: 17,
    messages: 4_947,
    agents: 700,
    documents: 682,
    attention: 183,
    peer_percent: 36,
};

fn scaled(count: usize, scale: f64) -> usize {
    ((count as f64 * scale).round() as usize).max(1)
}

/// The generated store for `scale`, from `keep` when an earlier run made it.
async fn generated_store(keep: &Path, scale: f64) -> PathBuf {
    let path = keep.join(format!("generated-{scale}.sqlite3"));
    if path.exists() {
        println!("using {}", path.display());
        return path;
    }
    // Each claim commits on its own, and a commit waits for a disk flush. Generate in memory-backed
    // storage when the host has room for it, then move the store in.
    let estimate = (scale * 3e9) as u64;
    let shm = Path::new("/dev/shm");
    let scratch = if shm.is_dir() && free_bytes(shm) > estimate * 2 {
        tempfile::tempdir_in(shm)
    } else {
        tempfile::tempdir_in(keep)
    }
    .unwrap();
    let partial = scratch.path().to_path_buf();
    let started = Instant::now();
    let (main, peer) = (partial.join("main.sqlite3"), partial.join("peer.sqlite3"));
    let (generated_main, generated_peer) = (main.clone(), peer.clone());
    tokio::task::spawn_blocking(move || {
        let peer_share = SAMPLED.peer_percent as f64 / 100.0;
        let peer = Store::open(&generated_peer, PEER).unwrap();
        peer.bind_fleet(FLEET).unwrap();
        generate(&peer, "peer", scale * peer_share);
        let store = Store::open(&generated_main, NODE).unwrap();
        store.bind_fleet(FLEET).unwrap();
        generate(&store, "host", scale * (1.0 - peer_share));
        replicate(&peer, &store);
    })
    .await
    .unwrap();
    // Reopening seals this node's own batches into replication envelopes, as a daemon start does.
    let sealed = main.clone();
    tokio::task::spawn_blocking(move || drop(Store::open(&sealed, NODE).unwrap()))
        .await
        .unwrap();
    // The messages go through the API, which is the only writer of their lifecycle.
    send_messages(&partial, &main, scaled(SAMPLED.messages, scale)).await;
    for suffix in ["", "-wal"] {
        let from = PathBuf::from(format!("{}{suffix}", main.display()));
        if from.exists() {
            std::fs::copy(&from, format!("{}{suffix}", path.display())).unwrap();
        }
    }
    drop(scratch);
    println!(
        "generated scale {scale} in {:.0}s: {}",
        started.elapsed().as_secs_f64(),
        path.display()
    );
    path
}

fn free_bytes(path: &Path) -> u64 {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `statvfs` fills the zeroed struct for a valid, nul-terminated path.
    unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stats) != 0 {
            return 0;
        }
        stats.f_bavail as u64 * stats.f_frsize as u64
    }
}

fn replicate(from: &Store, to: &Store) {
    loop {
        let before = to.index().unwrap();
        let exchange = from
            .export_replication_exchange(FLEET, &to.replication_inventory().unwrap())
            .unwrap();
        to.receive_replication_exchange(PEER, FLEET, &exchange)
            .unwrap();
        to.validate_replication_backlog().unwrap();
        to.apply_replication_repairs().unwrap();
        to.project_replication_backlog().unwrap();
        if to.index().unwrap() == before {
            break;
        }
    }
}

fn publish_mission(store: &Store, name: &str, steps: usize, revision: usize) {
    let mut source = format!(
        "version 2\nmission \"{name}\" state=\"ready\" {{\n  goal \"Ship invented change {revision} for {name}.\"\n  concurrent-runs max=1000000\n"
    );
    for step in 0..steps {
        source.push_str(&format!(
            "  step \"step-{step}\" {{\n    assigned-to \"agent/bench/seat-{}\"\n    goal \"Do invented part {step} of the change.\"\n  }}\n",
            step % 30
        ));
    }
    source.push_str("}\n");
    let intent = st3::parse_intent(&source, store_origin(store)).unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: None,
            },
        )
        .unwrap();
    let _ = store.apply(
        &intent,
        &planned.subject_tokens,
        &format!("bench-publish-{name}-{revision}"),
    );
}

fn store_origin(store: &Store) -> &str {
    store.origin()
}

/// Missions, their revisions, runs and their work, standing agents, documents, attention and
/// event claims in the sampled proportions.
fn generate(store: &Store, prefix: &str, scale: f64) {
    let missions = scaled(SAMPLED.missions, scale);
    for mission in 0..missions {
        let name = format!("bench/{prefix}-mission-{mission}");
        let steps = SAMPLED.steps_per_run - 1 + mission % 3;
        publish_mission(store, &name, steps, 0);
        // About every other mission has a second revision.
        if mission.is_multiple_of(2) {
            publish_mission(store, &name, steps, 1);
        }
    }
    let runs = scaled(SAMPLED.runs, scale);
    for run in 0..runs {
        let mission = format!("bench/{prefix}-mission-{}", run % missions);
        let Ok(view) = store.create_mission_run(&MissionRunRequest {
            mission,
            revision: None,
            workspace: format!("/srv/bench/{prefix}/runs/{run}"),
            requester: Some("person/bench-operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("bench-{prefix}-run-{run}"),
        }) else {
            continue;
        };
        for (index, step) in view.steps.iter().enumerate() {
            let _ = store.set_step_state(&step.subject, "ready", None);
            let worked = (run * 7 + index * 13) % 100 < SAMPLED.worked_percent;
            if worked {
                let actor = step
                    .assigned_to
                    .clone()
                    .unwrap_or_else(|| "agent/bench/seat-0".into());
                let request = |action: &str, n: usize, summary: bool| WorkRequest {
                    actor: Some(actor.clone()),
                    incarnation: Some(format!("{prefix}-{run}")),
                    summary: summary.then(|| format!("Invented {action} note {n}")),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("{}-{action}-{n}", step.subject),
                };
                let _ = store.work_action(&step.subject, "claim", &request("claim", 0, false));
                for n in 0..SAMPLED.renewals_per_worked_step {
                    let _ = store.work_action(&step.subject, "renew", &request("renew", n, true));
                }
                let _ = store.work_action(&step.subject, "progress", &request("progress", 0, true));
                let _ = store.work_action(&step.subject, "complete", &request("complete", 0, true));
            }
            let status = if (run + index) % 17 == 0 {
                "cancelled"
            } else {
                "completed"
            };
            let _ = store.set_step_state(&step.subject, status, None);
        }
        // All but a few runs have finished, as the reconciler finishes them.
        if run % 50 != 0 {
            for phase in ["cleanup-completed", "terminal"] {
                let _ = store.set_mission_run_state(&view.id, "completed", phase, None);
            }
        }
    }
    standing_agents(store, prefix, scaled(SAMPLED.agents, scale));
    for document in 0..scaled(SAMPLED.documents, scale) {
        let bytes = format!(
            "# Invented report {document}\n\n{}",
            "An invented finding.\n".repeat(700)
        );
        let _ = store.put_document(
            &format!("doc/bench/{prefix}/report-{}", document % 470),
            bytes.as_bytes(),
            &None,
            &format!("bench-{prefix}-document-{document}"),
        );
    }
    let asker = format!("agent/bench/{prefix}/standing-0");
    for request in 0..scaled(SAMPLED.attention, scale) {
        let posted = store.ask_person(&PersonAskRequest {
            legacy_request: None,
            person: "person/bench-operator".into(),
            title: format!("Invented question {request}"),
            reason: "An invented decision needs a person.".into(),
            actor: asker.clone(),
            step: None,
            new_run: Some(format!("question-{request}")),
            incarnation: None,
            idempotency_key: format!("bench-{prefix}-person-ask-{request}"),
        });
        if let Ok(posted) = posted {
            if request % 50 != 0 {
                let _ = store.finish_person_step(
                    &PersonStepResponse {
                        subject: posted.subject,
                        actor: "person/bench-operator".into(),
                        summary: "The invented decision is made".into(),
                        evidence: Vec::new(),
                        episode: None,
                        idempotency_key: format!("bench-{prefix}-person-done-{request}"),
                    },
                    false,
                );
            }
        }
    }
    let registry = st3_schema::registry();
    let mut refused = BTreeMap::<&str, usize>::new();
    let total = EVENT_KINDS.iter().map(|(_, count, _)| count).sum::<usize>();
    let mut remaining = EVENT_KINDS
        .iter()
        .map(|(kind, count, subjects)| (*kind, scaled(*count, scale), scaled(*subjects, scale)))
        .collect::<Vec<_>>();
    // A resource observation carries the resource's facts, about 7 KB in the sampled store: the
    // kind with the most facts, each text fact long.
    let resource = registry
        .resources
        .values()
        .max_by_key(|resource| resource.fields.len())
        .expect("the registry has resource kinds");
    let fact_length = 7_000 / resource.fields.len().max(1);
    // Interleave the kinds, as a fleet writes them.
    let mut written = 0_usize;
    let mut per_kind = BTreeMap::<&str, usize>::new();
    while remaining.iter().any(|(_, count, _)| *count > 0) {
        for (kind, count, subjects) in remaining.iter_mut() {
            if *count == 0 {
                continue;
            }
            *count -= 1;
            let spec = &registry.claims[*kind];
            let family = match spec.subjects.first().map(String::as_str) {
                Some("*") | None => "agent",
                Some(family) => family,
            };
            let index = per_kind.entry(kind).or_default();
            let subject = format!("{family}/bench/{prefix}-{}", *index % *subjects);
            *index += 1;
            let mut fields = synthetic_fields(spec, written);
            match *kind {
                "resource.observed" => {
                    fields.insert("kind".into(), Value::String(resource.kind.clone()));
                    for (name, fact) in &resource.fields {
                        if fact.value_type == ValueType::String && fact.values.is_empty() {
                            fields.insert(
                                name.clone(),
                                Value::String("invented fact ".repeat(fact_length / 14 + 1)),
                            );
                        }
                    }
                }
                "harness.timeline" => {
                    fields.insert("sequence".into(), Value::from(written as u64));
                }
                _ => {}
            }
            let input = ClaimInput {
                subject: subject.clone(),
                kind: (*kind).into(),
                actor: (family == "agent").then(|| subject.clone()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("bench-{prefix}-event-{written}")),
            };
            if let Err(error) = store.append_claim(&input) {
                if !refused.contains_key(kind) {
                    println!(
                        "{prefix}: {kind} refused: {} {}",
                        error.code,
                        error.message.chars().take(160).collect::<String>()
                    );
                }
                *refused.entry(kind).or_default() += 1;
            }
            written += 1;
        }
    }
    println!(
        "{prefix}: wrote {written} of {} event claims at scale {scale:.2}; refused {refused:?}",
        scaled(total, scale)
    );
}

/// Standing agents placed on another host, a third of them retired since.
fn standing_agents(store: &Store, prefix: &str, count: usize) {
    let declare = |agents: std::ops::Range<usize>, key: &str| {
        let mut source = "version 2\n".to_owned();
        for agent in agents {
            source.push_str(&format!(
                "agent \"bench/{prefix}/standing-{agent}\" {{\n  host \"bench-elsewhere\"\n  workspace \"/srv/bench/{prefix}/standing-{agent}\"\n  harness \"claude\" {{ }}\n}}\n"
            ));
        }
        let Ok(intent) = st3::parse_intent(&source, store.origin()) else {
            return;
        };
        let Ok(planned) = store.mission(
            &intent,
            IntentInput {
                kdl: source.clone(),
                source_name: Some(format!("bench-{prefix}-standing")),
            },
        ) else {
            return;
        };
        let _ = store.apply(&intent, &planned.subject_tokens, key);
    };
    declare(0..count, &format!("bench-{prefix}-standing-all"));
    declare(count / 3..count, &format!("bench-{prefix}-standing-retire"));
}

/// Fields for one claim of `spec`: every required field, and the optional ones every third
/// claim, with invented values of the declared types.
fn synthetic_fields(spec: &st3_schema::ClaimSpec, index: usize) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::new();
    for (name, field) in &spec.fields {
        if !field.required && !index.is_multiple_of(3) {
            continue;
        }
        let value = if let Some(first) = field.values.get(index % field.values.len().max(1)) {
            Value::String(first.clone())
        } else {
            match field.value_type {
                ValueType::Boolean => Value::Bool(index.is_multiple_of(2)),
                ValueType::Integer | ValueType::Number => Value::from(index as u64 % 100_000),
                ValueType::Array => json!([]),
                ValueType::Object => Value::Object(Map::new()),
                ValueType::SubjectReference => {
                    let family = field
                        .reference_families
                        .first()
                        .map(String::as_str)
                        .unwrap_or("agent");
                    Value::String(format!("{family}/bench/reference-{}", index % 97))
                }
                ValueType::String | ValueType::Any => {
                    Value::String(format!("invented {name} {}", index % 1_000))
                }
            }
        };
        fields.insert(name.clone(), value);
    }
    fields
}

fn claim_input(kind: &str, key: &str, index: usize, run: &str) -> ClaimInput {
    let registry = st3_schema::registry();
    let spec = &registry.claims[kind];
    let family = spec.subjects.first().map(String::as_str).unwrap_or("agent");
    let subject = format!("{family}/bench/live-{}", index % 7);
    let mut fields = synthetic_fields(spec, index);
    if kind == "lane.marked" && !run.is_empty() {
        fields.insert("entry".into(), Value::String(run.into()));
    }
    ClaimInput {
        subject: subject.clone(),
        kind: kind.into(),
        actor: (family == "agent").then_some(subject),
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(key.into()),
    }
}

/// Messages between seats, each delivered, most read and closed, through a daemon API on `main`.
async fn send_messages(scratch: &Path, main: &Path, count: usize) {
    let store = Arc::new(Store::open(main, NODE).unwrap());
    let socket = scratch.join("generate.sock");
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: NODE.into(),
        state_dir: scratch.join("state"),
        pty_root: scratch.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: Some(FLEET.into()),
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    };
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    while UnixStream::connect(&socket).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix(&socket);
    let mut sent = 0;
    for message in 0..count {
        let (from, to) = (
            format!("agent/bench/seat-{}", message % 30),
            format!("agent/bench/seat-{}", (message * 7 + 1) % 30),
        );
        let Ok(view) = client
            .post::<_, Value>(
                "/v1/messages",
                &json!({
                    "idempotency_key": format!("bench-generated-message-{message}"),
                    "from": from,
                    "to": to,
                    "title": format!("Invented note {message}"),
                    "content": "An invented update about an invented change, long enough to look like a status note between seats.",
                }),
            )
            .await
        else {
            continue;
        };
        sent += 1;
        let Some(subject) = view.get("subject").and_then(Value::as_str) else {
            continue;
        };
        let reference = urlencoding::encode(subject.trim_start_matches("message/")).into_owned();
        for (lifecycle, share) in [("delivered", 80), ("read", 79), ("closed", 90)] {
            if message % 100 < share {
                let _ = client
                    .post::<_, Value>(
                        &format!("/v1/messages/{reference}/claims"),
                        &json!({
                            "lifecycle": lifecycle,
                            "actor": to,
                            "evidence": [],
                            "idempotency_key": format!("bench-generated-{lifecycle}-{message}"),
                        }),
                    )
                    .await;
            }
        }
    }
    server.abort();
    println!("sent {sent} of {count} messages");
}
