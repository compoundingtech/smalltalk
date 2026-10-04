//! First sync: an empty node syncing about 130,000 envelopes of realistic shape from a peer over
//! loopback, with the time split by stage on both nodes.
//!
//! ```sh
//! TMPDIR=/var/tmp cargo test --release -p st3 --test integration first_sync:: -- --nocapture
//! ```
//!
//! A debug build skips it, as does a Nix build, whose sandbox is no place to time a sync.
//!
//! - `FIRST_SYNC_ENVELOPES` changes the store size.
//! - `FIRST_SYNC_DEADLINE_SECS` changes the two-minute limit, for example to profile a sync that
//!   does not finish within it yet.
//! - `FIRST_SYNC_MEMORY_STATE` keeps both stores in `/dev/shm`, to show the cost without disk
//!   flushes.
//! - `FIRST_SYNC_LINK_MBPS` sends each node's peer traffic through a link of that many
//!   megabits per second each way, such as a home Wi-Fi network, instead of bare loopback.
//! - `FIRST_SYNC_KEEP` keeps the stores for inspection.
//!
//! The two stores take about 1.5 GB under `TMPDIR`. Point it at a disk with room and a short
//! path, since the daemon sockets live there too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use st3::model::{
    ClaimInput, IntentInput, MissionRunRequest, ReplicationStatus, ReplicationTimings, WorkRequest,
};
use st3::store::Store;

const FLEET: &str = "5d0c6f8e-2b1a-4c3d-9e8f-7a6b5c4d3e2f";
// Below the Linux and macOS ephemeral port ranges, so no outgoing connection can hold them.
const SOURCE_PORT: u16 = 27_311;
const TARGET_PORT: u16 = 27_312;
/// With `FIRST_SYNC_LINK_MBPS`, each node dials the other through a slower link on these ports.
const LINK_TO_SOURCE_PORT: u16 = 27_313;
const LINK_TO_TARGET_PORT: u16 = 27_314;

/// Two steps on one standing agent. The agent is not declared, so the source daemon starts no
/// runtimes while the target syncs.
const MISSION: &str = r#"version 2
mission "bench/build" state="ready" {
  goal "Build one invented change."
  concurrent-runs max=1000000
  step "plan" {
    assigned-to "agent/bench/builder"
    goal "Plan the change."
  }
  step "build" {
    assigned-to "agent/bench/builder"
    goal "Build the change."
  }
}"#;

/// Lease renewals per claimed step. With the run, step and work transitions, about a quarter
/// of the claims are about runs and a tenth are renewals, as in a sampled fleet store.
const RENEWALS_PER_STEP: usize = 5;

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_node_syncs_its_peer_within_two_minutes() {
    if cfg!(debug_assertions) {
        println!("skipped: a debug build is too slow to measure; run with cargo test --release");
        return;
    }
    if std::env::var_os("NIX_BUILD_TOP").is_some() {
        println!("skipped: a Nix build sandbox is no place to time a sync");
        return;
    }
    let target_envelopes = env_number("FIRST_SYNC_ENVELOPES", 130_000);
    let deadline = Duration::from_secs(env_number("FIRST_SYNC_DEADLINE_SECS", 120));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    if std::env::var_os("FIRST_SYNC_KEEP").is_some() {
        println!("keeping {}", root.display());
        std::mem::forget(temp);
    }
    let secret = &root.join("fleet.secret");
    write_private(secret, &"7".repeat(64));
    let path_dir = &root.join("path");
    std::fs::create_dir_all(path_dir).unwrap();
    // The daemon needs a `pty` on PATH. Nothing here starts a runtime.
    write_executable(&path_dir.join("pty"), "#!/bin/sh\nexit 1\n");

    // `FIRST_SYNC_MEMORY_STATE` keeps both stores in memory, where a commit never waits for a
    // disk flush, to show what a sync costs without them.
    let memory = std::env::var_os("FIRST_SYNC_MEMORY_STATE")
        .map(|_| tempfile::tempdir_in("/dev/shm").unwrap());
    let memory_root = memory.as_ref().map(|memory| memory.path());
    let link = std::env::var("FIRST_SYNC_LINK_MBPS")
        .ok()
        .and_then(|mbps| mbps.parse::<f64>().ok());
    let (to_source, to_target) = match link {
        Some(mbps) => {
            let bytes_per_second = mbps * 1_000_000.0 / 8.0;
            start_link(LINK_TO_SOURCE_PORT, SOURCE_PORT, bytes_per_second).await;
            start_link(LINK_TO_TARGET_PORT, TARGET_PORT, bytes_per_second).await;
            println!("peers talk over a {mbps} Mbit/s link each way");
            (LINK_TO_SOURCE_PORT, LINK_TO_TARGET_PORT)
        }
        None => (SOURCE_PORT, TARGET_PORT),
    };
    let source_dir = node_dir(
        &root,
        memory_root,
        "source",
        SOURCE_PORT,
        "target",
        to_target,
        secret,
    );
    let target_dir = node_dir(
        &root,
        memory_root,
        "target",
        TARGET_PORT,
        "source",
        to_source,
        secret,
    );

    // The generator writes one claim per transaction, and each commit waits for a disk flush.
    // Build the source store in memory-backed storage when the host has some, then copy it in.
    let started = Instant::now();
    let scratch = match Path::new("/dev/shm") {
        shm if shm.is_dir() => tempfile::tempdir_in(shm),
        _ => tempfile::tempdir(),
    }
    .unwrap();
    let mix = generate(&scratch.path().join("claims.sqlite3"), target_envelopes);
    for suffix in ["", "-wal"] {
        let from = scratch.path().join(format!("claims.sqlite3{suffix}"));
        if from.exists() {
            std::fs::copy(
                &from,
                source_dir.join(format!("state/claims.sqlite3{suffix}")),
            )
            .unwrap();
        }
    }
    drop(scratch);
    println!(
        "generated {} claims in {:.1}s: {}",
        mix.values().sum::<usize>(),
        started.elapsed().as_secs_f64(),
        mix.iter()
            .map(|(kind, count)| format!("{kind}={count}"))
            .collect::<Vec<_>>()
            .join(" ")
    );

    let _source = Node::start(&source_dir, path_dir);
    let source = st3::client::Client::unix(source_dir.join("run/st3.sock"));
    // Seal and export the source's envelopes before the clock starts.
    let sealed = Instant::now();
    let source_status = wait_for(&source, Duration::from_secs(600), |status| {
        status.received_envelopes as usize >= mix.values().sum::<usize>()
    })
    .await
    .expect("the source sealed its envelopes");
    println!(
        "source sealed {} envelopes in {:.1}s",
        source_status.received_envelopes,
        sealed.elapsed().as_secs_f64()
    );

    let clock = Instant::now();
    let _target = Node::start(&target_dir, path_dir);
    let target = st3::client::Client::unix(target_dir.join("run/st3.sock"));
    let mut last_report = Instant::now();
    let mut outcome = None;
    while clock.elapsed() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (Ok(source_status), Ok(target_status)) = (
            source
                .get::<ReplicationStatus>("/v1/replication/status")
                .await,
            target
                .get::<ReplicationStatus>("/v1/replication/status")
                .await,
        ) else {
            continue;
        };
        if last_report.elapsed() >= Duration::from_secs(10) {
            last_report = Instant::now();
            println!(
                "{:>5.1}s target holds {} of {} envelopes",
                clock.elapsed().as_secs_f64(),
                target_status.received_envelopes,
                source_status.received_envelopes
            );
        }
        // Receipt matches the digests; admission then turns every envelope into records, and
        // projection brings the graph up to date.
        let admitted = target_status.valid_records
            + target_status.invalid_records
            + target_status.unknown_records;
        if target_status.authority_digest == source_status.authority_digest
            && target_status.graph_digest == source_status.graph_digest
            && admitted >= source_status.valid_records
            && target_status.pending_records == 0
            && target_status.unhealthy_projections == 0
        {
            outcome = Some((clock.elapsed(), source_status, target_status));
            break;
        }
    }
    let (source_status, target_status) = match &outcome {
        Some((_, source_status, target_status)) => (source_status.clone(), target_status.clone()),
        None => (
            source.get("/v1/replication/status").await.unwrap(),
            target.get("/v1/replication/status").await.unwrap(),
        ),
    };
    let elapsed = outcome
        .as_ref()
        .map_or(clock.elapsed(), |(elapsed, ..)| *elapsed);
    println!(
        "\n{} after {:.1}s: target holds {} of {} envelopes ({:.0} envelopes/s)",
        if outcome.is_some() {
            "synced"
        } else {
            "stopped"
        },
        elapsed.as_secs_f64(),
        target_status.received_envelopes,
        source_status.received_envelopes,
        target_status.received_envelopes as f64 / elapsed.as_secs_f64()
    );
    report("target", &target_status.timings, elapsed);
    report("source", &source_status.timings, elapsed);
    report_signing(source_status.received_envelopes);

    assert!(
        outcome.is_some(),
        "the target did not sync within {}s",
        deadline.as_secs()
    );
    // Each node keeps writing a few observations of its own, such as its transport status. Let
    // both exchange those before comparing what they hold; this is not part of the sync time.
    let (source_status, target_status) = settle(&source, &target).await;
    assert_eq!(
        (target_status.invalid_records, target_status.unknown_records),
        (0, 0),
        "the target must end with exactly its peer's valid records"
    );
    assert_eq!(target_status.valid_records, source_status.valid_records);
    assert_eq!(
        target_status.graph_digest, source_status.graph_digest,
        "the target must project the same graph as its peer"
    );
}

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Fill a store with about `total` claims, one per envelope, mixed the way a sampled fleet
/// store is. Returns the claim count by kind.
fn generate(path: &Path, total: usize) -> BTreeMap<String, usize> {
    let store = Store::open(path, "source").unwrap();
    store.bind_fleet(FLEET).unwrap();
    let intent = st3::parse_intent(MISSION, "source").unwrap();
    let mission = store
        .mission(
            &intent,
            IntentInput {
                kdl: MISSION.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &mission.subject_tokens, "bench-mission")
        .unwrap();

    let workspace = path.parent().unwrap().display().to_string();
    let mut run_index = 0_usize;
    let mut event_index = 0_usize;
    let mut harness_writes = BTreeMap::new();
    while store.index().unwrap() < total as u64 {
        // A run every 80 events, so about a quarter of the claims are about runs.
        if event_index.is_multiple_of(80) {
            run_cycle(&store, &workspace, run_index);
            run_index += 1;
        }
        append_event(&store, event_index, &mut harness_writes);
        event_index += 1;
    }
    let mut mix = BTreeMap::new();
    let mut after = 0;
    loop {
        let page = store
            .claims_page(None, None, after, None, false, 500)
            .unwrap();
        if page.claims.is_empty() {
            break;
        }
        for claim in &page.claims {
            *mix.entry(claim.kind.clone()).or_default() += 1;
        }
        after = page.claims.last().unwrap().store_index;
    }
    mix
}

/// One run: both steps claimed, renewed, progressed and completed, then the run finished.
fn run_cycle(store: &Store, workspace: &str, index: usize) {
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "bench/build".into(),
            revision: None,
            workspace: workspace.into(),
            requester: Some("person/bench-operator".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: format!("bench-run-{index}"),
        })
        .unwrap();
    for step in &run.steps {
        let request = |action: &str, n: usize| WorkRequest {
            actor: Some("agent/bench/builder".into()),
            incarnation: Some(format!("builder-{index}")),
            // A renewal with a summary publishes its claim, as a lease near expiry does.
            summary: Some(format!("Invented {action} summary")),
            reason: None,
            evidence: Vec::new(),
            idempotency_key: format!("{}-{action}-{n}", step.subject),
        };
        store.set_step_state(&step.subject, "ready", None).unwrap();
        store
            .work_action(&step.subject, "claim", &request("claim", 0))
            .unwrap();
        for n in 0..RENEWALS_PER_STEP {
            store
                .work_action(&step.subject, "renew", &request("renew", n))
                .unwrap();
        }
        store
            .work_action(&step.subject, "progress", &request("progress", 0))
            .unwrap();
        store
            .work_action(&step.subject, "complete", &request("complete", 0))
            .unwrap();
        store
            .set_step_state(&step.subject, "completed", None)
            .unwrap();
    }
    // Finish the run as the reconciler does, so the source has nothing left to reconcile.
    for phase in ["cleanup-completed", "terminal"] {
        store
            .set_mission_run_state(&run.id, "completed", phase, None)
            .unwrap();
    }
}

/// One event claim, cycling through the event kinds in their sampled proportions. Harness
/// timelines, a tenth of a sampled store written by older builds, now stay in the writer's local
/// log and never replicate, so they are left out.
fn append_event(store: &Store, index: usize, harness_writes: &mut BTreeMap<String, usize>) {
    const KINDS: &[(&str, usize)] = &[
        ("harness.observed", 17),
        ("loop.state", 10),
        ("observer.observed", 6),
        ("harness.usage", 4),
        ("transport.observed", 4),
        ("runtime.observed", 3),
        ("message.sent", 3),
    ];
    let weight = KINDS.iter().map(|(_, weight)| weight).sum::<usize>();
    let mut slot = index % weight;
    let kind = KINDS
        .iter()
        .find(|(_, weight)| {
            let here = slot < *weight;
            slot = slot.saturating_sub(*weight);
            here
        })
        .unwrap()
        .0;
    let agent = format!("agent/bench/worker-{:02}", index % 24);
    // Harness state and usage replicate only when they change, so each agent's next write of
    // either kind differs from its last one.
    let writes = harness_writes.entry(format!("{agent} {kind}")).or_default();
    *writes += 1;
    let writes = *writes;
    let now = 1_790_000_000_000_u64 + index as u64 * 1_000;
    let (subject, actor, fields) = match kind {
        "harness.observed" => (
            agent.clone(),
            Some(agent.clone()),
            json!({"driver": "claude", "state": if writes.is_multiple_of(2) { "working" } else { "idle" }}),
        ),
        "loop.state" => (
            format!("loop-run/bench/loop-{}", index % 40),
            None,
            json!({"round": index / 40, "status": "running"}),
        ),
        "observer.observed" => (
            format!("observer/bench/repository-{}", index % 8),
            None,
            json!({
                "changed": index.is_multiple_of(7),
                "cursor": format!("cursor-{index}"),
                "next_check_unix_ms": (now + 60_000).to_string(),
                "revision": format!("revision-{index}"),
                "status": "ok",
            }),
        ),
        "harness.usage" => (
            agent.clone(),
            Some(agent.clone()),
            json!({
                "compactions": writes,
                "context_used_percent": (index % 100) as f64 / 2.0,
                "context_used_tokens": index % 200_000,
                "context_window_tokens": 200_000,
                "driver": "claude",
                "incarnation_id": format!("incarnation-{}", index % 24),
                "model": "invented-model",
                "semantics": "context_occupancy",
            }),
        ),
        "transport.observed" => (
            format!("host/bench-peer-{}", index % 3),
            None,
            json!({"last_success_at": now, "protocol": "http-replication", "status": "up"}),
        ),
        "runtime.observed" => (
            format!("exec/bench/check-{}", index % 16),
            None,
            json!({
                "adopted": false,
                "host": "source",
                "reachability": "reachable",
                "reason": Value::Null,
                "runtime_id": format!("runtime-{index}"),
                "shutdown_timeout_ms": 5_000,
                "status": "exited",
                "terminal": false,
            }),
        ),
        _ => (
            format!("message/bench-{index}"),
            Some("agent/bench/worker-00".to_owned()),
            json!({
                "content": "An invented status update about the build.",
                "from": "agent/bench/worker-00",
                "in_reply_to": Value::Null,
                "status": "sent",
                "tags": ["bench"],
                "title": "Build status",
                "to": "person/bench-operator",
            }),
        ),
    };
    store
        .append_claim(&ClaimInput {
            subject,
            kind: kind.into(),
            actor,
            fields: fields
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("bench-event-{index}")),
        })
        .unwrap();
}

/// Forward connections on `listen` to `upstream`, sending at most `bytes_per_second` each way on
/// each connection.
async fn start_link(listen: u16, upstream: u16, bytes_per_second: f64) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", listen))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((inbound, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await
                else {
                    return;
                };
                let (inbound_read, inbound_write) = inbound.into_split();
                let (outbound_read, outbound_write) = outbound.into_split();
                tokio::join!(
                    pump(inbound_read, outbound_write, bytes_per_second),
                    pump(outbound_read, inbound_write, bytes_per_second),
                );
            });
        }
    });
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    bytes_per_second: f64,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut buffer = vec![0_u8; 64 * 1024];
    while let Ok(read) = from.read(&mut buffer).await {
        if read == 0 || to.write_all(&buffer[..read]).await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_secs_f64(read as f64 / bytes_per_second)).await;
    }
    let _ = to.shutdown().await;
}

fn node_dir(
    root: &Path,
    memory: Option<&Path>,
    name: &str,
    port: u16,
    peer: &str,
    peer_port: u16,
    secret: &Path,
) -> PathBuf {
    let dir = root.join(name);
    for sub in ["run", "pty", "home"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    match memory {
        Some(memory) => {
            let state = memory.join(name);
            std::fs::create_dir_all(&state).unwrap();
            std::os::unix::fs::symlink(&state, dir.join("state")).unwrap();
        }
        None => std::fs::create_dir_all(dir.join("state")).unwrap(),
    }
    std::fs::write(
        dir.join("config.toml"),
        format!(
            r#"node = "{name}"
fleet_id = "{FLEET}"
shared_secret_file = "{}"
state_dir = "{}"
pty_root = "{}"
socket = "{}"
client_gateway_socket = "{}"
peer_listen = "127.0.0.1:{port}"

[[peers]]
name = "{peer}"
url = "http://127.0.0.1:{peer_port}"
"#,
            secret.display(),
            dir.join("state").display(),
            dir.join("pty").display(),
            dir.join("run/st3.sock").display(),
            dir.join("run/st3-client.sock").display(),
        ),
    )
    .unwrap();
    dir
}

/// One node's daemon and replication worker, stopped on drop.
struct Node(Vec<Child>);

impl Node {
    fn start(dir: &Path, path_dir: &Path) -> Self {
        let spawn = |args: &[&str], log: &str| {
            st3::test_support::command(env!("CARGO_BIN_EXE_st3-fixture"))
                .args(args)
                .arg("--config")
                .arg(dir.join("config.toml"))
                .env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", path_dir.display()))
                .env("HOME", dir.join("home"))
                .env("XDG_RUNTIME_DIR", dir.join("run"))
                .env("XDG_STATE_HOME", dir.join("home"))
                .env("XDG_CONFIG_HOME", dir.join("home"))
                .env("RUST_LOG", "warn")
                .stdin(Stdio::null())
                .stdout(std::fs::File::create(dir.join(log)).unwrap())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap()
        };
        let daemon = spawn(&["up"], "daemon.log");
        let socket = dir.join("run/st3.sock");
        let started = Instant::now();
        // Opening a large store replays its graph before the socket appears.
        while !socket.exists() && started.elapsed() < Duration::from_secs(600) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            socket.exists(),
            "the {} daemon did not start",
            dir.display()
        );
        let worker = spawn(&["replication-worker"], "worker.log");
        Self(vec![worker, daemon])
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Both nodes' status once they hold the same records and graph, or after a minute.
async fn settle(
    source: &st3::client::Client,
    target: &st3::client::Client,
) -> (ReplicationStatus, ReplicationStatus) {
    let started = Instant::now();
    loop {
        let source_status: ReplicationStatus = source.get("/v1/replication/status").await.unwrap();
        let target_status: ReplicationStatus = target.get("/v1/replication/status").await.unwrap();
        let settled = source_status.authority_digest == target_status.authority_digest
            && source_status.valid_records == target_status.valid_records
            && source_status.graph_digest == target_status.graph_digest;
        if settled || started.elapsed() > Duration::from_secs(60) {
            return (source_status, target_status);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for(
    client: &st3::client::Client,
    limit: Duration,
    ready: impl Fn(&ReplicationStatus) -> bool,
) -> Option<ReplicationStatus> {
    let started = Instant::now();
    while started.elapsed() < limit {
        if let Ok(status) = client
            .get::<ReplicationStatus>("/v1/replication/status")
            .await
            && ready(&status)
        {
            return Some(status);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

fn report(node: &str, timings: &ReplicationTimings, elapsed: Duration) {
    let wall = elapsed.as_millis().max(1) as f64;
    let share = |ms: u64| format!("{ms:>7} ms {:>5.1}%", ms as f64 * 100.0 / wall);
    println!(
        "\n{node}: {} exchanges received, {} envelopes received",
        timings.exchanges, timings.envelopes_received
    );
    for (stage, ms) in [
        ("round trips", timings.round_trip_ms),
        ("export", timings.export_ms),
        ("snapshot", timings.snapshot_ms),
        ("receipt", timings.receipt_ms),
        ("admission", timings.admission_ms),
        ("  verify", timings.verify_ms),
        ("projection", timings.projection_ms),
        ("repair", timings.repair_ms),
        ("signing", timings.signing_ms),
        ("sqlite", timings.sqlite_ms),
        ("  commits", timings.commit_ms),
    ] {
        println!("  {stage:<12} {}", share(ms));
    }
    let busy = timings.export_ms
        + timings.snapshot_ms
        + timings.receipt_ms
        + timings.admission_ms
        + timings.projection_ms
        + timings.repair_ms;
    println!(
        "  {} commits; store stages busy {}, the rest of the wall time waiting",
        timings.commits,
        share(busy).trim_start()
    );
}

/// Envelopes are signed by their writer's member key and verified by every receiver. The
/// benchmark fleet is unkeyed, so measure that work for the same number of envelopes here.
fn report_signing(envelopes: u64) {
    let (key, _) = st3::fleet::MemberKey::generate().unwrap();
    let messages = (0..envelopes)
        .map(|sequence| {
            st3::fleet::envelope_signature_message(FLEET, "source", sequence, &"a".repeat(64))
        })
        .collect::<Vec<_>>();
    let started = Instant::now();
    let signatures = messages
        .iter()
        .map(|message| key.sign(message))
        .collect::<Vec<_>>();
    let signing = started.elapsed();
    let started = Instant::now();
    for (message, signature) in messages.iter().zip(&signatures) {
        assert!(st3::fleet::verify_signature(
            key.public(),
            message,
            signature
        ));
    }
    println!(
        "\nmember-key signatures for {envelopes} envelopes: signing {:.0} ms, verifying {:.0} ms",
        signing.as_secs_f64() * 1_000.0,
        started.elapsed().as_secs_f64() * 1_000.0
    );
}

fn write_private(path: &Path, contents: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
