#![cfg(unix)]
//! The claim log diet under load.
//!
//! An isolated daemon runs busy harnesses, loops waiting on a person, and a file observer. Each
//! simulated harness driver posts its state every second, and while it works it also posts a
//! timeline entry every two seconds and a usage reading every three. Each harness also sends a
//! message every minute. The daemon must replicate at most a fifth of the claims that main would
//! and lose no durable fact: every message, every harness state change, each harness's final
//! usage and every loop's gate reach a second node, which ends with the same graph.
//!
//! On main every accepted observation is a claim, so main replicates the claims this build
//! replicates, less the latest-state claims it published, plus every row of the local
//! observation log. That estimate leaves out the loop state main rewrites while a loop waits on
//! its gate, so it understates main.
//!
//! Both counts start after the workload's missions are published, which main and this build
//! record alike. The suite runs the workload for 90 seconds. The measurement in
//! `doc/fleet/smalltalk/claim-log-diet` is the ten-minute run:
//!
//! ```sh
//! cargo test -p st3 --test log_diet -- --ignored --nocapture
//! ```
//!
//! Inside an st3 harness, unset `ST_AGENT` first: the local API binds a caller to the agent in
//! its process ancestry, and these drivers write as their own agents.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};
use st3::api::AppState;
use st3::client::{Client, Endpoint};
use st3::model::{ClaimInput, ClaimRecord, IntentInput, MissionRunRequest};
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::{Store, local_observation_position};
use tokio::sync::{Notify, watch};

const NODE: &str = "diet-node";
const FLEET: &str = "5b0e1f6a-2c1d-4d8e-9f3a-7c6b5a4d3e2f";
const HARNESSES: usize = 6;
const LOOPS: usize = 2;
/// A harness works for 30 seconds of each 40.
const TURN_SECONDS: u64 = 40;
const WORKING_SECONDS: u64 = 30;

/// A runtime that starts nothing: the workload drives the harness API directly.
struct NoRuntime;

impl RuntimeControl for NoRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> Result<Option<RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    fn remove(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn attach(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn screen(&self, _: &str) -> Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

fn workload_source(workspace: &Path) -> String {
    let mut source =
        String::from("version 2\n\nresource \"diet/config\" { kind \"filesystem.file\" }\n\n");
    source.push_str(&format!(
        r#"mission "diet/observer" state="ready" {{
  goal "Observe a file that keeps changing."
  completion {{ when "all-steps-exhausted" }}
  observer "config" {{
    resource "resource/diet/config"
    provider "local.file"
    locator "{}/config.toml"
    field "status"
    field "content_hash"
  }}
  step "wait" {{
    agentless
    gate "stop observing" type="human" {{
      reviewer "person/example"
      question "Stop observing the file?"
    }}
  }}
}}
"#,
        workspace.display()
    ));
    for index in 0..LOOPS {
        source.push_str(&format!(
            r#"
mission "diet/loop-{index}" state="ready" {{
  goal "Wait for a person between rounds."
  completion {{ when "all-steps-exhausted" }}
  loop "improve" {{
    max-rounds 2
    until {{
      gate "accept" type="human" {{
        reviewer "person/example"
        question "Accept this round?"
      }}
    }}
    round {{ completion {{ when "all-steps-exhausted" }} }}
  }}
}}
"#
        ));
    }
    source
}

struct Daemon {
    store: Arc<Store>,
    client: Client,
    server: tokio::task::JoinHandle<()>,
    reconciling: Arc<AtomicBool>,
    reconciler: tokio::task::JoinHandle<()>,
}

impl Daemon {
    async fn start(root: &Path) -> Self {
        let store = Arc::new(Store::open(&root.join("claims.sqlite3"), NODE).unwrap());
        let notify = Arc::new(Notify::new());
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let state = AppState {
            store: store.clone(),
            notify: notify.clone(),
            event_notify: watch::channel(0_u64).0,
            node: NODE.into(),
            state_dir,
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let socket = root.join("st3.sock");
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            let _ = st3::api::serve_unix(&server_socket, st3::api::router(state)).await;
        });
        // Reconcile off the async runtime after each change, and at least four times a second,
        // until `stop`.
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            Arc::new(NoRuntime),
            NODE.into(),
            notify.clone(),
        ));
        let reconciling = Arc::new(AtomicBool::new(true));
        let running = reconciling.clone();
        let reconciler = tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                let pass = reconciler.clone();
                tokio::task::spawn_blocking(move || pass.reconcile_once())
                    .await
                    .unwrap()
                    .unwrap();
                let _ = tokio::time::timeout(Duration::from_millis(250), notify.notified()).await;
            }
        });
        let started = Instant::now();
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the API never listened"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            store,
            client: Client::new(Endpoint::Unix(socket)),
            server,
            reconciling,
            reconciler,
        }
    }

    /// Stop reconciling and serving before the store's directory goes away.
    async fn stop(self) {
        self.reconciling.store(false, Ordering::SeqCst);
        self.reconciler.await.unwrap();
        self.server.abort();
        let _ = self.server.await;
    }

    fn publish(&self, source: &str, workspace: &Path) -> Vec<String> {
        let intent = st3::parse_intent(source, NODE).unwrap();
        let plan = self
            .store
            .mission(
                &intent,
                IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
        self.store
            .apply(&intent, &plan.subject_tokens, "diet-workload")
            .unwrap();
        let mut missions = vec!["diet/observer".to_owned()];
        missions.extend((0..LOOPS).map(|index| format!("diet/loop-{index}")));
        missions
            .iter()
            .map(|mission| {
                self.store
                    .create_mission_run(&MissionRunRequest {
                        mission: mission.clone(),
                        revision: None,
                        workspace: workspace.display().to_string(),
                        requester: Some("person/example".into()),
                        mode: Some("run".into()),
                        inputs: BTreeMap::new(),
                        idempotency_key: format!("diet-run:{mission}"),
                    })
                    .unwrap()
                    .id
            })
            .collect()
    }

    async fn post(&self, input: ClaimInput) -> ClaimRecord {
        self.client.post("/v1/claims", &input).await.unwrap()
    }
}

/// One simulated harness driver.
struct Harness {
    subject: String,
    offset: u64,
    transitions: u64,
    state: &'static str,
    since_ms: u64,
    context_tokens: u64,
    total_tokens: u64,
    entry: u64,
    last_usage: BTreeMap<String, Value>,
}

impl Harness {
    fn new(index: usize) -> Self {
        Self {
            subject: format!("agent/diet/worker-{index}"),
            offset: index as u64 * 7,
            transitions: 0,
            state: "",
            since_ms: 0,
            context_tokens: 1_000,
            total_tokens: 0,
            entry: 0,
            last_usage: BTreeMap::new(),
        }
    }

    fn input(&self, kind: &str, fields: BTreeMap<String, Value>, key: String) -> ClaimInput {
        ClaimInput {
            subject: self.subject.clone(),
            kind: kind.into(),
            actor: Some(self.subject.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key),
        }
    }

    fn observed(&self, now_ms: u64) -> ClaimInput {
        self.input(
            "harness.observed",
            BTreeMap::from([
                ("state".into(), Value::String(self.state.into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
                ("observed_since_ms".into(), Value::from(self.since_ms)),
                ("observed_at_ms".into(), Value::from(now_ms)),
                ("transition_sequence".into(), Value::from(self.transitions)),
            ]),
            format!("diet-observed:{}:{now_ms}", self.subject),
        )
    }

    fn usage(&self, semantics: &str) -> ClaimInput {
        let tokens = if semantics == "session_cumulative" {
            ("total_tokens", self.total_tokens)
        } else {
            ("context_used_tokens", self.context_tokens)
        };
        self.input(
            "harness.usage",
            BTreeMap::from([
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
                ("semantics".into(), Value::String(semantics.into())),
                (tokens.0.into(), Value::from(tokens.1)),
                ("model".into(), Value::String("example-model".into())),
                ("compactions".into(), Value::from(0)),
            ]),
            format!("diet-usage:{}:{semantics}:{}", self.subject, tokens.1),
        )
    }

    fn timeline(&self) -> ClaimInput {
        self.input(
            "harness.timeline",
            BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                (
                    "entry_id".into(),
                    Value::String(format!("{}/entry-{}", self.subject, self.entry)),
                ),
                ("sequence".into(), Value::from(self.entry)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("assistant".into())),
                ("entry_type".into(), Value::String("content".into())),
                ("final".into(), Value::Bool(true)),
                (
                    "body".into(),
                    json!({"media_type": "text/plain", "text": format!("step {}", self.entry)}),
                ),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
            ]),
            format!("diet-timeline:{}:{}", self.subject, self.entry),
        )
    }

    /// Post one second of driver output. `last` settles every harness idle.
    async fn tick(&mut self, daemon: &Daemon, second: u64, last: bool) -> Vec<ClaimRecord> {
        let now_ms = 1_800_000_000_000 + second * 1_000;
        let working = !last && (second + self.offset) % TURN_SECONDS < WORKING_SECONDS;
        let state = if working { "working" } else { "idle" };
        if state != self.state {
            self.state = state;
            self.since_ms = now_ms;
            self.transitions += 1;
        }
        let mut written = vec![daemon.post(self.observed(now_ms)).await];
        if working && second % 2 == 0 {
            self.entry += 1;
            written.push(daemon.post(self.timeline()).await);
        }
        if working && second % 3 == 0 {
            self.context_tokens += 1_500;
            self.total_tokens += 2_000;
            for semantics in ["context_occupancy", "session_cumulative"] {
                let reading = self.usage(semantics);
                self.last_usage
                    .insert(semantics.into(), json!(reading.fields.clone()));
                written.push(daemon.post(reading).await);
            }
        }
        if second % 60 == 0 {
            written.push(
                daemon
                    .client
                    .post(
                        "/v1/claims",
                        &ClaimInput {
                            subject: format!("message/diet-{}-{second}", self.offset),
                            kind: "message.sent".into(),
                            actor: Some(self.subject.clone()),
                            fields: BTreeMap::from([
                                ("from".into(), Value::String(self.subject.clone())),
                                ("to".into(), Value::String("person/example".into())),
                                (
                                    "content".into(),
                                    Value::String(format!("progress at {second}s")),
                                ),
                                ("status".into(), Value::String("sent".into())),
                            ]),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!(
                                "diet-message:{}:{second}",
                                self.subject
                            )),
                        },
                    )
                    .await
                    .unwrap(),
            );
        }
        written
    }
}

struct Report {
    seconds: u64,
    replicated: u64,
    main_estimate: u64,
    local_rows: u64,
    by_kind: BTreeMap<String, (u64, u64)>,
}

fn claims(store: &Store) -> Vec<ClaimRecord> {
    let mut claims = Vec::new();
    let mut after = 0;
    loop {
        let page = store
            .claims_page(None, None, after, None, false, 500)
            .unwrap();
        let done = page.claims.len() < 500;
        after = page.claims.last().map_or(after, |claim| claim.store_index);
        claims.extend(page.claims);
        if done {
            return claims;
        }
    }
}

/// Replicate `source` into `target` until both hold the same authority.
fn converge(source: &Store, target: &Store) {
    source.bind_fleet(FLEET).unwrap();
    target.bind_fleet(FLEET).unwrap();
    for _ in 0..1_000 {
        let inventory = target.replication_inventory().unwrap();
        if inventory.digest == source.replication_inventory().unwrap().digest {
            return;
        }
        let exchange = source
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        target
            .receive_replication_exchange(NODE, FLEET, &exchange)
            .unwrap();
        target.validate_replication_backlog().unwrap();
        target.apply_replication_repairs().unwrap();
        target.project_replication_backlog().unwrap();
    }
    panic!("the replica never converged");
}

async fn run_workload(seconds: u64) -> Report {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("config.toml"), "version = 0\n").unwrap();
    let daemon = Daemon::start(root.path()).await;
    let runs = daemon.publish(&workload_source(&workspace), &workspace);
    let setup_index = daemon.store.index().unwrap();

    let mut harnesses = (0..HARNESSES).map(Harness::new).collect::<Vec<_>>();
    let mut state_changes = BTreeMap::<String, Vec<ClaimRecord>>::new();
    let mut messages = Vec::new();
    let started = Instant::now();
    for second in 0..=seconds {
        let last = second == seconds;
        for harness in &mut harnesses {
            let before = harness.transitions;
            for record in harness.tick(&daemon, second, last).await {
                if record.kind == "message.sent" {
                    messages.push(record);
                } else if record.kind == "harness.observed" && harness.transitions != before {
                    assert!(
                        local_observation_position(&record).is_none(),
                        "a state change replicates at once"
                    );
                    state_changes
                        .entry(harness.subject.clone())
                        .or_default()
                        .push(record);
                }
            }
        }
        if second % 90 == 45 {
            std::fs::write(
                workspace.join("config.toml"),
                format!("version = {second}\n"),
            )
            .unwrap();
        }
        let next = Duration::from_secs(second + 1);
        if let Some(wait) = next.checked_sub(started.elapsed()) {
            tokio::time::sleep(wait).await;
        }
    }

    let store = daemon.store.clone();
    daemon.stop().await;
    let all = claims(&store);
    let workload = all
        .iter()
        .filter(|claim| claim.store_index > setup_index)
        .collect::<Vec<_>>();
    let local = store.local_observations_after(0, usize::MAX).unwrap();
    let latest_claims = workload
        .iter()
        .filter(|claim| matches!(claim.kind.as_str(), "harness.observed" | "harness.usage"))
        .count() as u64;
    // Per kind: (what main would replicate, what this build replicated). Main replicates
    // every observation the local log holds and every other claim this build replicated.
    let mut by_kind = BTreeMap::<String, (u64, u64)>::new();
    for claim in &workload {
        by_kind.entry(claim.kind.clone()).or_default().1 += 1;
    }
    for observation in &local {
        by_kind.entry(observation.kind.clone()).or_default().0 += 1;
    }
    for (main, replicated) in by_kind.values_mut() {
        if *main == 0 {
            *main = *replicated;
        }
    }

    // No durable fact is lost: a second node receives every message, every harness state
    // change, each harness's final usage and every loop's gate, and ends with the same graph.
    let replica = Store::open_memory("diet-replica").unwrap();
    converge(&store, &replica);
    let status = |store: &Store| {
        let status = store.replication_status(true, Some(FLEET), &[]).unwrap();
        (status.authority_digest, status.graph_digest)
    };
    assert_eq!(status(&store), status(&replica));
    let replicated_ids = claims(&replica)
        .into_iter()
        .map(|claim| (claim.id.clone(), claim))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(replicated_ids.len(), all.len());
    for message in &messages {
        assert_eq!(
            replicated_ids.get(&message.id).map(|claim| &claim.body),
            Some(&message.body),
            "message {} reached the replica unchanged",
            message.subject
        );
    }
    for harness in &harnesses {
        let replicated_states = replica
            .claims_for(&harness.subject, Some("harness.observed"))
            .unwrap();
        let posted = &state_changes[&harness.subject];
        assert_eq!(
            replicated_states
                .iter()
                .map(|claim| claim.id.clone())
                .collect::<Vec<_>>(),
            posted
                .iter()
                .map(|claim| claim.id.clone())
                .collect::<Vec<_>>(),
            "every state change of {} and nothing else replicated",
            harness.subject
        );
        assert_eq!(
            replicated_states.last().unwrap().body["fields"]["state"],
            "idle"
        );
        let usage = replica
            .claims_for(&harness.subject, Some("harness.usage"))
            .unwrap();
        for (semantics, fields) in &harness.last_usage {
            let latest = usage
                .iter()
                .rev()
                .find(|claim| claim.body["fields"]["semantics"] == semantics.as_str())
                .unwrap_or_else(|| panic!("{} has no {semantics} usage", harness.subject));
            assert_eq!(
                &latest.body["fields"], fields,
                "the final {semantics} reading of {} replicated",
                harness.subject
            );
        }
        assert!(
            replica
                .timeline_claims_for_incarnation_at(&harness.subject, "inc-1", None, false, 10)
                .unwrap()
                .claims
                .is_empty(),
            "a timeline stays on its node"
        );
    }
    for run in runs.iter().skip(1) {
        let run = replica.mission_run(run).unwrap().unwrap();
        let loop_subject = format!(
            "loop-run/{}/improve",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        assert!(
            replica
                .gate_request_for_owner(&loop_subject)
                .unwrap()
                .is_some(),
            "{} asked for its gate on the replica",
            run.id
        );
        assert!(
            replica
                .claims_for(&loop_subject, Some("loop.state"))
                .unwrap()
                .len()
                <= 3,
            "a waiting loop does not rewrite its state"
        );
    }

    let replicated = workload.len() as u64;
    Report {
        seconds,
        replicated,
        main_estimate: replicated - latest_claims + local.len() as u64,
        local_rows: local.len() as u64,
        by_kind,
    }
}

fn check(report: &Report) {
    println!(
        "{} s: replicated {} claims; main would replicate at least {}; {} local observations ({:.1}%)",
        report.seconds,
        report.replicated,
        report.main_estimate,
        report.local_rows,
        100.0 * report.replicated as f64 / report.main_estimate as f64
    );
    println!("kind | main (at least) | replicated");
    for (kind, (main, replicated)) in &report.by_kind {
        println!("{kind} | {main} | {replicated}");
    }
    assert!(
        report.replicated * 5 <= report.main_estimate,
        "replicated {} of at least {} claims main would",
        report.replicated,
        report.main_estimate
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_daemon_replicates_at_most_a_fifth_of_what_main_would() {
    check(&run_workload(90).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "the ten-minute measurement; run with --ignored"]
async fn ten_busy_minutes_replicate_at_most_a_fifth_of_what_main_would() {
    check(&run_workload(600).await);
}
