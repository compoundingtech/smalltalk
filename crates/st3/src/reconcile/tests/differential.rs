//! Incremental passes against full passes.
//!
//! Two stores receive the same events: on one the reconciler evaluates every item on every pass,
//! on the other it skips items whose inputs did not change and which are not due. After each event
//! both reconcile until quiet and their claims must match. The incremental side is also strict, so
//! a periodic full pass there that has to correct anything fails too. Events are drawn from a
//! seeded generator: runs, the resource a gate reads, human verdicts, clock jumps past deadlines and
//! timeouts, restarts of the reconciler with its memory lost, and writes from outside it. See
//! `doc/fleet/smalltalk/idle-cpu-incremental-design`.

use super::*;
use crate::model::MissionRunRequest;

const START: u128 = 1_900_000_000_000;

const SEAT: &str = r#"version 2
agent "worker" { workspace "/tmp"; command "true" }
"#;

const STOP_SEAT: &str = r#"version 2
stop "agent/node.worker"
"#;

const SOURCE: &str = r#"version 2
agent "worker" { workspace "/tmp"; command "true" }
mission "diff/flow" state="ready" timeout="2h" {
  goal "Exercise the reconciler."
  concurrent-runs max=4
  completion { when "all-steps-exhausted" }
  step "prepare" {
    agentless
    gate "config" { field "status" "resource/diff/config" is "ready" }
  }
  step "review" {
    agentless
    depends-on { step "prepare" completed }
    gate "accept" type="human" {
      reviewer "person/example"
      question "Accept this?"
    }
  }
  step "wait" timeout="10m" {
    agentless
    gate "signal" { field "status" "resource/diff/signal" is "ready" }
  }
  step "finish" {
    agentless
    depends-on { step "review" completed }
  }
  step "work" {
    assigned-to "agent/worker"
    depends-on { step "prepare" completed }
  }
  step "team" {
    agentless
    depends-on { step "review" completed }
    agent "helper" { workspace "/tmp"; command "true"; restart "never" }
  }
}
"#;

/// A small deterministic generator, so a failing sequence can be replayed from its seed.
struct Draw(u64);

impl Draw {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

struct Side {
    store: Arc<Store>,
    /// Outlives restarts, as the PTY service outlives the daemon.
    runtime: Arc<FakeRuntime>,
    reconciler: Reconciler<FakeRuntime>,
    /// How many of the runtime's starts already have a terminal.
    started: std::cell::Cell<usize>,
    /// How many of the runtime's stops and kills already ended their terminals.
    ended: std::cell::Cell<(usize, usize)>,
    /// Whether terminals ignore a stop and wait for the kill.
    hang: std::cell::Cell<bool>,
}

impl Side {
    fn new(incremental: bool) -> Self {
        let store = Arc::new(Store::open_memory("node").unwrap());
        smallclaims::store::set_thread_clock(Some(START));
        store.set_write_clock_at(START).unwrap();
        apply_source(&store, SOURCE, "diff-flow");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Self::reconciler(&store, &runtime, incremental);
        Self {
            store,
            runtime,
            reconciler,
            started: std::cell::Cell::new(0),
            ended: std::cell::Cell::new((0, 0)),
            hang: std::cell::Cell::new(false),
        }
    }

    fn reconciler(
        store: &Arc<Store>,
        runtime: &Arc<FakeRuntime>,
        incremental: bool,
    ) -> Reconciler<FakeRuntime> {
        let mut reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.skip_unneeded = incremental;
        reconciler
    }

    /// A new reconciler over the same store: everything it kept in memory is gone.
    fn restart(&mut self, incremental: bool) {
        self.reconciler = Self::reconciler(&self.store, &self.runtime, incremental);
    }

    /// A terminal for each start since the last look, as the PTY service makes one per start.
    fn terminals_start(&self) {
        let started = self.runtime.started_members.lock().unwrap().clone();
        let mut ptys = self.runtime.ptys.lock().unwrap();
        for member in &started[self.started.get()..] {
            ptys.retain(|pty| pty.runtime_id != member.runtime_id);
            ptys.push(RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("current".into()),
            });
        }
        self.started.set(started.len());
        // A stop ends its terminal unless terminals hang; a kill always does.
        let stops = self.runtime.stops.lock().unwrap().clone();
        let kills = self.runtime.kills.lock().unwrap().clone();
        let (stopped, killed) = self.ended.get();
        let mut ended = kills[killed..].to_vec();
        if !self.hang.get() {
            ended.extend_from_slice(&stops[stopped..]);
        }
        for pty in ptys.iter_mut() {
            if ended.contains(&pty.runtime_id) && pty.status == "running" {
                pty.status = "exited".into();
                pty.exit_code = Some(0);
            }
        }
        self.ended.set((stops.len(), kills.len()));
    }

    /// Every started member's terminal runs again.
    fn terminals_run(&self) {
        let started = self.runtime.started_members.lock().unwrap().clone();
        let mut ptys = self.runtime.ptys.lock().unwrap();
        for member in started {
            ptys.retain(|pty| pty.runtime_id != member.runtime_id);
            ptys.push(RuntimeObservation {
                runtime_id: member.runtime_id,
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("current".into()),
            });
        }
    }

    /// Every terminal exits, or vanishes from the PTY service without a word.
    fn terminals_end(&self, vanish: bool) {
        let mut ptys = self.runtime.ptys.lock().unwrap();
        if vanish {
            ptys.clear();
        }
        for pty in ptys.iter_mut() {
            pty.status = "exited".into();
            pty.exit_code = Some(0);
        }
    }

    /// Reconcile until a pass writes nothing.
    fn settle(&self) {
        let mut before = self.store.index().unwrap();
        for _ in 0..30 {
            self.reconciler.reconcile_once().unwrap();
            self.terminals_start();
            let after = self.store.index().unwrap();
            if after == before {
                return;
            }
            before = after;
        }
        let last = self
            .store
            .changes_since(before.saturating_sub(3), i64::MAX)
            .unwrap()
            .changes
            .into_iter()
            .map(|change| format!("{} {} {}", change.subject, change.kind, change.body))
            .collect::<Vec<_>>();
        panic!("reconciling did not settle; it kept writing {last:#?}");
    }

    /// Every claim, without what differs only by when or in which order it was written.
    fn claims(&self) -> Vec<String> {
        let id = regex_lite_hex();
        let mut claims = self
            .store
            .changes_since(0, 0)
            .unwrap()
            .changes
            .into_iter()
            .map(|change| {
                let mut body: Value = serde_json::from_str(&change.body).unwrap_or(Value::Null);
                if let Some(object) = body.as_object_mut() {
                    object.remove("evidence");
                }
                id(&format!(
                    "{} {} {} {}",
                    change.subject,
                    change.kind,
                    change.actor.unwrap_or_default(),
                    body
                ))
            })
            .collect::<Vec<_>>();
        claims.sort();
        claims
    }
}

/// Replaces 32- and 64-digit hex identifiers, which hash write order and time.
fn regex_lite_hex() -> impl Fn(&str) -> String {
    |text: &str| {
        let mut out = String::with_capacity(text.len());
        let mut run = String::new();
        let flush = |run: &mut String, out: &mut String| {
            if run.len() >= 32 {
                out.push_str("<id>");
            } else {
                out.push_str(run);
            }
            run.clear();
        };
        for character in text.chars() {
            if character.is_ascii_hexdigit() && !character.is_ascii_uppercase() {
                run.push(character);
            } else {
                flush(&mut run, &mut out);
                out.push(character);
            }
        }
        flush(&mut run, &mut out);
        out
    }
}

fn observe(store: &Store, resource: &str, status: &str, key: String) {
    store
        .append_claim(&ClaimInput {
            subject: format!("resource/diff/{resource}"),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("custom.st3.test".into())),
                ("facts".into(), serde_json::json!({ "status": status })),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .unwrap();
}

/// Answer the first open human review, in the same way on both stores.
fn answer_review(store: &Store, verdict: &str, at: u128) {
    let mut runs = store.active_mission_run_ids_for_origin("node").unwrap();
    runs.sort();
    for run in runs {
        let Some(view) = store.mission_run(&run).unwrap() else {
            continue;
        };
        for step in &view.steps {
            if step.step != "review" || step.status != "working" {
                continue;
            }
            let Some(request) = store.gate_request_for_owner(&step.subject).unwrap() else {
                continue;
            };
            store
                .append_claim(&ClaimInput {
                    subject: request.subject.clone(),
                    kind: "gate.result".into(),
                    actor: Some("person/example".into()),
                    fields: BTreeMap::from([
                        ("verdict".into(), Value::String(verdict.into())),
                        ("request".into(), Value::String(request.id.clone())),
                    ]),
                    evidence: vec![request.id.clone()],
                    expected_subject: None,
                    idempotency_key: Some(format!("verdict:{}:{at}", request.subject)),
                })
                .unwrap();
            return;
        }
    }
}

fn start_run(store: &Store, at: u128) {
    let _ = store.create_mission_run(&MissionRunRequest {
        mission: "diff/flow".into(),
        revision: None,
        workspace: "/tmp".into(),
        requester: Some("person/example".into()),
        mode: Some("run".into()),
        inputs: BTreeMap::new(),
        idempotency_key: format!("run:{at}"),
    });
}

/// Act on the first unfinished agent step as the worker. Fails the same way on both stores when
/// the step is not in a state that allows it.
fn work(store: &Store, action: &str, at: u128) {
    let mut runs = store.active_mission_run_ids_for_origin("node").unwrap();
    runs.sort();
    for run in runs {
        let Some(view) = store.mission_run(&run).unwrap() else {
            continue;
        };
        let Some(step) = view.steps.iter().find(|step| {
            step.step == "work" && ["ready", "claimed", "working"].contains(&step.status.as_str())
        }) else {
            continue;
        };
        // `take` claims and reports progress at once, as a worker that starts at once does, and
        // `give back` claims and releases.
        let actions = match action {
            "take" => vec!["claim", "progress"],
            "give back" => vec!["claim", "release"],
            action => vec![action],
        };
        for action in actions {
            let request = crate::model::WorkRequest {
                actor: Some("agent/node.worker".into()),
                incarnation: Some("current".into()),
                // A bare renewal writes no claim, only the lease: a change the feed never shows.
                summary: (action != "renew").then(|| format!("{action} at {at}")),
                reason: None,
                evidence: Vec::new(),
                idempotency_key: format!("{action}:{at}"),
            };
            let _ = store.work_action(&step.subject, action, &request);
        }
        return;
    }
}

/// The worker's harness reports its state, as its driver does on every turn edge.
fn harness(store: &Store, state: &str, at: u128) {
    store
        .append_claim(&ClaimInput {
            subject: "agent/node.worker".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/node.worker".into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String(state.into())),
                ("driver".into(), Value::String("claude".into())),
                ("incarnation_id".into(), Value::String("current".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("harness:{state}:{at}")),
        })
        .unwrap();
}

/// The worker takes the next step of each open message to it: delivered, then read.
fn read_mail(store: &Store, at: u128) {
    for message in store.messages(Some("agent/node.worker"), false).unwrap() {
        let (kind, status) = match message.status.as_str() {
            "sent" | "staged" => ("message.delivered", "delivered"),
            "delivered" => ("message.read", "read"),
            _ => continue,
        };
        store
            .append_claim(&ClaimInput {
                subject: message.subject.clone(),
                kind: kind.into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{kind}:{}:{at}", message.subject)),
            })
            .unwrap();
    }
}

/// Claims written on another member, delivered through replication.
struct Peer {
    store: Store,
    writes: u64,
}

impl Peer {
    /// The other member observes a resource the runs read, possibly a while before it arrives.
    fn observe(&mut self, resource: &str, status: &str, at: u128) {
        self.store.set_write_clock_at(at).unwrap();
        observe(
            &self.store,
            resource,
            status,
            format!("peer:{}", self.writes),
        );
        self.writes += 1;
    }

    /// Everything written so far, or only the newest batch, which waits on the earlier ones.
    fn deliver(&self, sides: [&Side; 2], newest_only: bool) {
        let after = if newest_only {
            self.writes.saturating_sub(1)
        } else {
            0
        };
        let batch = self.store.export_replication(after).unwrap();
        for side in sides {
            let _ = side.store.import_replication("peer", &batch);
        }
    }
}

fn noise(store: &Store, at: u128) {
    store
        .append_claim(&ClaimInput {
            subject: format!("resource/diff/unrelated-{}", at % 3),
            kind: "resource.observed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("kind".into(), Value::String("custom.st3.test".into())),
                ("facts".into(), serde_json::json!({ "at": at.to_string() })),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("noise:{at}")),
        })
        .unwrap();
}

/// Returns the claim kinds the sequence wrote, and the step statuses it reached.
fn run_sequence(seed: u64, events: usize) -> BTreeSet<String> {
    let mut draw = Draw(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut full = Side::new(false);
    let mut incremental = Side::new(true);
    let mut peer = Peer {
        store: Store::open_memory("peer").unwrap(),
        writes: 0,
    };
    let mut now = START;
    let mut log = Vec::new();
    for step in 0..events {
        smallclaims::store::set_thread_clock(Some(now));
        for side in [&full, &incremental] {
            side.store.set_write_clock_at(now).unwrap();
        }
        let event = draw.below(23);
        let label = match event {
            0 | 1 => {
                for side in [&full, &incremental] {
                    start_run(&side.store, now);
                }
                "start a run".to_owned()
            }
            2 | 3 => {
                let status = if draw.below(2) == 0 {
                    "ready"
                } else {
                    "pending"
                };
                let resource = if draw.below(2) == 0 {
                    "config"
                } else {
                    "signal"
                };
                for side in [&full, &incremental] {
                    observe(
                        &side.store,
                        resource,
                        status,
                        format!("{resource}:{status}:{now}"),
                    );
                }
                format!("{resource} {status}")
            }
            4 => {
                let verdict = if draw.below(4) == 0 { "fail" } else { "pass" };
                for side in [&full, &incremental] {
                    answer_review(&side.store, verdict, now);
                }
                format!("review {verdict}")
            }
            5 | 6 => {
                let jump = [
                    1_000,
                    30_000,
                    5 * 60_000,
                    11 * 60_000,
                    20 * 60_000,
                    3_600_000,
                ][draw.below(6) as usize];
                now += jump;
                smallclaims::store::set_thread_clock(Some(now));
                for side in [&full, &incremental] {
                    side.store.set_write_clock_at(now).unwrap();
                }
                format!("clock +{jump}ms")
            }
            7 => {
                full.restart(false);
                incremental.restart(true);
                "restart".to_owned()
            }
            8..=10 | 18 | 19 | 22 => {
                let action = [
                    "claim",
                    "take",
                    "give back",
                    "renew",
                    "progress",
                    "complete",
                    "fail",
                    "release",
                ][draw.below(8) as usize];
                for side in [&full, &incremental] {
                    work(&side.store, action, now);
                }
                format!("work {action}")
            }
            11 => {
                let label = match draw.below(3) {
                    0 => {
                        full.terminals_run();
                        incremental.terminals_run();
                        "terminals run"
                    }
                    1 => {
                        full.terminals_end(false);
                        incremental.terminals_end(false);
                        "terminals exit"
                    }
                    _ => {
                        full.terminals_end(true);
                        incremental.terminals_end(true);
                        "terminals vanish"
                    }
                };
                label.to_owned()
            }
            16 if draw.below(2) == 0 => {
                let state = ["ready", "working", "idle"][draw.below(3) as usize];
                for side in [&full, &incremental] {
                    harness(&side.store, state, now);
                }
                format!("harness {state}")
            }
            17 => {
                for side in [&full, &incremental] {
                    read_mail(&side.store, now);
                }
                "worker reads its mail".to_owned()
            }
            14 => {
                let hang = draw.below(2) == 0;
                for side in [&full, &incremental] {
                    side.hang.set(hang);
                }
                format!("terminals hang on stop: {hang}")
            }
            15 => {
                let (source, label) = if draw.below(4) == 0 {
                    (STOP_SEAT, "stop the seat")
                } else {
                    (SEAT, "declare the seat")
                };
                for side in [&full, &incremental] {
                    apply_source(&side.store, source, &format!("{label}:{now}"));
                }
                label.to_owned()
            }
            12 | 13 => {
                let status = if draw.below(2) == 0 {
                    "ready"
                } else {
                    "pending"
                };
                let resource = if draw.below(2) == 0 {
                    "config"
                } else {
                    "signal"
                };
                let lag = [0, 5_000, 20 * 60_000][draw.below(3) as usize];
                peer.observe(resource, status, now.saturating_sub(lag));
                match draw.below(3) {
                    0 => format!("peer {resource} {status}, held"),
                    1 => {
                        peer.deliver([&full, &incremental], true);
                        format!("peer {resource} {status}, delivered alone")
                    }
                    _ => {
                        peer.deliver([&full, &incremental], false);
                        format!("peer {resource} {status}, delivered")
                    }
                }
            }
            _ => {
                for side in [&full, &incremental] {
                    noise(&side.store, now);
                }
                "unrelated write".to_owned()
            }
        };
        log.push(label);
        full.settle();
        incremental.settle();
        let (expected, actual) = (full.claims(), incremental.claims());
        if expected != actual {
            let missing = expected
                .iter()
                .filter(|claim| !actual.contains(claim))
                .take(8)
                .collect::<Vec<_>>();
            let extra = actual
                .iter()
                .filter(|claim| !expected.contains(claim))
                .take(8)
                .collect::<Vec<_>>();
            smallclaims::store::set_thread_clock(None);
            panic!(
                "seed {seed}, event {step}: incremental passes diverged from full passes\n\
                 events: {log:?}\nmissing: {missing:#?}\nextra: {extra:#?}"
            );
        }
    }
    smallclaims::store::set_thread_clock(None);
    let mut reached = full
        .store
        .changes_since(0, 0)
        .unwrap()
        .changes
        .into_iter()
        .map(|change| change.kind)
        .collect::<BTreeSet<_>>();
    for view in full.store.mission_runs().unwrap() {
        reached.insert(format!("run {}", view.status));
        for step in view.steps {
            reached.insert(format!("{} {}", step.step, step.status));
        }
    }
    reached
}

#[test]
fn incremental_passes_write_what_full_passes_write() {
    let seeds = std::env::var("ST3_DIFFERENTIAL_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    let mut reached = BTreeSet::new();
    for seed in 1..=seeds {
        reached.extend(run_sequence(seed, 100));
    }
    // The sequences must keep reaching what they exist to compare.
    let missing = [
        "gate.result",
        "resource.observed",
        "runtime.observed",
        "work.claimed",
        "work.progress",
        "work.released",
        "work.submitted",
        "review failed",
        "wait failed",
        "work completed",
        "runtime.action.requested",
        "runtime.action.deadline-reached",
        "runtime.action.succeeded",
        "harness.observed",
        "message.delivered",
        "message.read",
        "run completed",
        "run failed",
    ]
    .into_iter()
    .filter(|expected| !reached.contains(*expected))
    .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "no sequence reached {missing:?}: {reached:#?}"
    );
}
