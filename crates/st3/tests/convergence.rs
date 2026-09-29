//! Checkpoints under random schedules.
//!
//! Each run starts three to five nodes from a seed and drives them with a seeded scheduler:
//! writes of every claim kind the registry lets a store accept, a person's claims, exchanges
//! between random pairs that deliver a random part of each exchange in a random order, partitions
//! and heals, nodes offline for days, clock skew of up to three days, checkpoint work (seal,
//! verify, trim) at random points, crashes at every trim boundary, restarts, excusals by a
//! person, new empty nodes, and an old-build node that never takes part in checkpoints.
//!
//! An oracle node receives every envelope the moment it is written and never trims. After the
//! schedule, the run heals every partition, brings every node back, and exchanges and runs
//! checkpoint work until nothing moves. Then every node must hold:
//!
//! 1. the same authority digest, among nodes that take part in checkpoints;
//! 2. the oracle's graph digest;
//! 3. the oracle's reader answers, for every subject;
//! 4. every claim of a kind no rule drops, and every person's claim, with the oracle's body;
//! 5. the same trimmed checkpoint and the same tombstones, field by field;
//! 6. no invalid record, and no record still pending;
//! 7. for every claim it lacks, a tombstone, and only for claims a rule may drop.
//!
//! Also, no cut has two certificates unless people excused each side of a partition, no proof
//! fails, no trim finds the graph changed, and no node's committed index ever moves back.
//!
//! On a failure the suite prints the seed and the schedule. To explore further:
//!
//! ```sh
//! cargo test -p st3 --test integration convergence::                   # the fixed seeds
//! CONVERGENCE_SEED=42 cargo test -p st3 --test integration -- --ignored --nocapture convergence::one_seed
//! CONVERGENCE_RUNS=200 cargo test -p st3 --test integration -- --ignored --nocapture convergence::explore
//! ```
//!
//! Stores run in this process, each in its own file, and exchange through the same export and
//! receipt calls the replication worker makes. An old build here is a node that never runs
//! checkpoint work and never adopts a manifest; `tests/fleet.rs` covers the pinned old release.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use st3::model::{ClaimInput, ClaimRecord};
use st3::store::{
    CheckpointAction, CheckpointContext, CheckpointExcuseRequest, Store, TrimFault, certificates,
    checkpoint_name, stable_checkpoints,
};
use st3_schema::{FieldSpec, Retention, ValueType};

const FLEET: &str = "7d3f9a2e-5b6c-4e1d-8a0f-2c9b8e7d6f5a";
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const PERSON: &str = "person/operator";

/// The seeds CI runs. Every failing seed found by hand is added here.
const SEEDS: &[u64] = &[
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597,
];

/// Kinds a checkpoint rule may drop. Every other kind is kept on every node.
const RULE_KINDS: &[&str] = &[
    "harness.observed",
    "harness.timeline",
    "loop.state",
    "subscription.mission-deferred",
    "observer.observed",
    "daemon.diagnostic",
    "transport.observed",
    "runtime.action.requested",
    "runtime.action.succeeded",
    "runtime.action.failed",
    "runtime.action.deadline-reached",
    "render.applied",
    "runtime.readiness-deadline-reached",
];

/// A small, fast, seeded generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below(index + 1);
            items.swap(index, other);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct Node {
    name: String,
    path: PathBuf,
    scratch: PathBuf,
    store: Store,
    online: bool,
    /// Never seals, verifies, trims or adopts: a build from before checkpoints.
    old_build: bool,
    /// How far this node's clock runs ahead of the simulation's.
    skew_ms: i64,
    index_high_water: u64,
}

/// Deliberate bugs, each switched on for a whole run, that the checks must catch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sabotage {
    None,
    /// Delete what a checkpoint drops, then forget its tombstones.
    ForgetTombstones,
    /// Adopt a manifest that is missing a tombstone, bypassing verification.
    PartialAdoption,
}

struct World {
    seed: u64,
    rng: Rng,
    root: tempfile::TempDir,
    nodes: Vec<Node>,
    oracle: Store,
    /// The partition group of each node, by index. Nodes exchange only within a group.
    groups: Vec<usize>,
    /// Simulated days since the run started.
    days: i64,
    schedule: Vec<String>,
    failures: Vec<String>,
    serial: u64,
    /// Claims people wrote, by ID.
    person_claims: BTreeSet<String>,
    /// Whether a person excused a writer while the fleet was partitioned.
    excused_while_partitioned: bool,
    written: BTreeMap<String, usize>,
    sabotage: Sabotage,
}

impl World {
    fn new(seed: u64, sabotage: Sabotage) -> Self {
        let root = tempfile::tempdir().unwrap();
        let oracle = Store::open(&root.path().join("oracle.sqlite3"), "oracle").unwrap();
        oracle.bind_fleet(FLEET).unwrap();
        let mut world = Self {
            seed,
            rng: Rng(seed),
            root,
            nodes: Vec::new(),
            oracle,
            groups: Vec::new(),
            days: 0,
            schedule: Vec::new(),
            failures: Vec::new(),
            serial: 0,
            person_claims: BTreeSet::new(),
            excused_while_partitioned: false,
            written: BTreeMap::new(),
            sabotage,
        };
        let count = 3 + world.rng.below(3);
        let old = world.rng.chance(50).then(|| world.rng.below(count));
        for index in 0..count {
            world.add_node(Some(index) == old);
        }
        world
    }

    fn note(&mut self, line: String) {
        self.schedule.push(line);
    }

    fn fail(&mut self, message: String) {
        self.failures.push(message);
    }

    fn add_node(&mut self, old_build: bool) {
        let names = ["alder", "birch", "cedar", "dogwood", "elm", "fir"];
        let name = names[self.nodes.len()].to_owned();
        let path = self.root.path().join(format!("{name}.sqlite3"));
        let scratch = self.root.path().join(format!("{name}-scratch"));
        let store = Store::open(&path, name.clone()).unwrap();
        store.bind_fleet(FLEET).unwrap();
        let skew_ms = if self.rng.chance(30) {
            (self.rng.below(6 * 24) as i64 - 3 * 24) * DAY_MS / 24
        } else {
            0
        };
        self.nodes.push(Node {
            name: name.clone(),
            path,
            scratch,
            store,
            online: true,
            old_build,
            skew_ms,
            index_high_water: 0,
        });
        self.groups.push(0);
        let index = self.nodes.len() - 1;
        self.set_clock(index);
        self.note(format!(
            "add {name}{} skew {}h",
            if old_build { " (old build)" } else { "" },
            skew_ms / (DAY_MS / 24)
        ));
    }

    fn offset(&self, index: usize) -> i64 {
        self.days * DAY_MS + self.nodes[index].skew_ms
    }

    fn set_clock(&self, index: usize) {
        self.nodes[index]
            .store
            .set_write_clock_offset(self.offset(index))
            .unwrap();
    }

    fn context(&self, index: usize) -> CheckpointContext {
        let node = &self.nodes[index];
        CheckpointContext {
            now_unix_ms: (now_ms() + self.offset(index)) as u128,
            configured_peers: self
                .nodes
                .iter()
                .filter(|other| other.name != node.name)
                .map(|other| other.name.clone())
                .collect(),
            scratch: node.scratch.clone(),
            reviewer: PERSON.into(),
        }
    }

    /// The committed index must never move back, across trims and restarts alike.
    fn check_index(&mut self, index: usize) {
        let current = self.nodes[index].store.index().unwrap();
        let node = &mut self.nodes[index];
        if current < node.index_high_water {
            let message = format!(
                "{}'s committed index moved back from {} to {current}",
                node.name, node.index_high_water
            );
            self.fail(message);
        } else {
            self.nodes[index].index_high_water = current;
        }
    }

    fn restart(&mut self, index: usize) {
        let path = self.nodes[index].path.clone();
        let name = self.nodes[index].name.clone();
        // Close the old store before opening the file again, as a process exit would.
        let placeholder = Store::open_memory(format!("{name}-closed")).unwrap();
        drop(std::mem::replace(&mut self.nodes[index].store, placeholder));
        self.nodes[index].store = Store::open(&path, name).unwrap();
        self.nodes[index].store.bind_fleet(FLEET).unwrap();
        self.set_clock(index);
        self.check_index(index);
    }

    /// Send the oracle everything this node holds that the oracle lacks.
    fn tap(&mut self, index: usize) {
        for _ in 0..50 {
            let node = &self.nodes[index].store;
            let exchange = node
                .export_replication_exchange(FLEET, &self.oracle.replication_inventory().unwrap())
                .unwrap();
            if exchange.envelopes.is_empty() {
                return;
            }
            let receipt = self
                .oracle
                .receive_replication_exchange(&self.nodes[index].name, FLEET, &exchange)
                .unwrap();
            if let Err(error) = admit(&self.oracle) {
                self.fail(format!("the oracle could not admit: {error:#}"));
                return;
            }
            if receipt.received == 0 {
                return;
            }
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.serial += 1;
        format!("{prefix}-{}-{}", self.seed, self.serial)
    }

    /// Deliver part of what `from` would send `to`, in a random order. Returns how many
    /// envelopes `to` stored.
    fn exchange(&mut self, from: usize, to: usize, whole: bool) -> usize {
        let inventory = self.nodes[to].store.replication_inventory().unwrap();
        let mut exchange = self.nodes[from]
            .store
            .export_replication_exchange(FLEET, &inventory)
            .unwrap();
        if !whole && !exchange.envelopes.is_empty() {
            self.rng.shuffle(&mut exchange.envelopes);
            let keep = 1 + self.rng.below(exchange.envelopes.len());
            exchange.envelopes.truncate(keep);
        }
        let name = self.nodes[from].name.clone();
        let target = &self.nodes[to].store;
        let received = match target.receive_replication_exchange(&name, FLEET, &exchange) {
            Ok(receipt) => receipt.received,
            Err(error) => {
                let message = format!(
                    "{} refused an exchange from {name}: {}: {}",
                    self.nodes[to].name, error.code, error.message
                );
                self.fail(message);
                return 0;
            }
        };
        if let Err(error) = admit(target) {
            let message = format!("{} could not admit: {error:#}", self.nodes[to].name);
            self.fail(message);
        }
        self.check_index(to);
        self.adopt(from, to);
        received
    }

    /// What the replication worker does after an exchange: when `from` advertises a checkpoint
    /// `to` needs, fetch its manifest from `from` and adopt it.
    fn adopt(&mut self, from: usize, to: usize) -> bool {
        if self.nodes[to].old_build {
            return false;
        }
        let Some(advertised) = self.nodes[from].store.trimmed_checkpoint().unwrap() else {
            return false;
        };
        let own = self.nodes[to].store.trimmed_checkpoint().unwrap();
        if own
            .as_ref()
            .is_some_and(|own| *own == advertised || own.cut_unix_ms > advertised.cut_unix_ms)
        {
            return false;
        }
        let Some(need) = self.nodes[to].store.checkpoint_manifest_need().unwrap() else {
            return false;
        };
        if need.checkpoint != advertised.id || need.drop_digest != advertised.drop_digest {
            return false;
        }
        let mut manifest = self.nodes[from]
            .store
            .checkpoint_manifest(&need.checkpoint, need.cut_unix_ms)
            .unwrap();
        if self.sabotage == Sabotage::PartialAdoption && !manifest.claims.is_empty() {
            manifest.claims.pop();
            manifest.envelopes.pop();
            // A broken adopter that skips verification: record what it was given as is.
            let actions = self.nodes[to]
                .store
                .adopt_checkpoint_unverified_for_tests(&manifest)
                .unwrap();
            self.note(format!(
                "{} adopted a partial {} from {}",
                self.nodes[to].name, need.checkpoint, self.nodes[from].name
            ));
            self.check_actions(to, &actions);
            return true;
        }
        match self.nodes[to].store.adopt_checkpoint(&manifest) {
            Ok(actions) => {
                self.note(format!(
                    "{} adopts {} from {}",
                    self.nodes[to].name, need.checkpoint, self.nodes[from].name
                ));
                self.check_actions(to, &actions);
                self.check_index(to);
                !actions.is_empty()
            }
            Err(error) => {
                let message = format!(
                    "{} could not adopt {} from {}: {}: {}",
                    self.nodes[to].name,
                    need.checkpoint,
                    self.nodes[from].name,
                    error.code,
                    error.message
                );
                self.fail(message);
                false
            }
        }
    }

    fn check_actions(&mut self, index: usize, actions: &[CheckpointAction]) {
        for action in actions {
            match action {
                CheckpointAction::ProofFailed { .. }
                | CheckpointAction::TrimGraphChanged { .. } => {
                    let message = format!("{}: {action:?}", self.nodes[index].name);
                    self.fail(message);
                }
                _ => {}
            }
        }
    }

    /// One pass of checkpoint work, sometimes stopped at a trim boundary as a crash would.
    fn checkpoint_work(&mut self, index: usize, faults: bool) -> Vec<CheckpointAction> {
        if faults && self.rng.chance(25) {
            let fault = match self.rng.below(4) {
                0 => TrimFault::AfterTombstones,
                1 => TrimFault::AfterChunk(1),
                2 => TrimFault::AfterChunk(2),
                _ => TrimFault::BeforeFinish,
            };
            self.nodes[index].store.set_trim_chunk_envelopes(2);
            self.nodes[index].store.set_trim_fault(Some(fault));
            self.note(format!(
                "{} will crash at {fault:?}",
                self.nodes[index].name
            ));
        }
        let context = self.context(index);
        let result = self.nodes[index].store.checkpoint_step(&context);
        // An armed fault that did not fire stays armed only for this pass.
        self.nodes[index].store.set_trim_fault(None);
        let actions = match result {
            Ok(actions) => actions,
            Err(error) if error.to_string().contains("for a test") => {
                let name = self.nodes[index].name.clone();
                self.note(format!("{name} crashed: {error}"));
                self.restart(index);
                self.nodes[index]
                    .store
                    .set_trim_chunk_envelopes(st3::store::TRIM_CHUNK_ENVELOPES);
                return Vec::new();
            }
            Err(error) => {
                let message = format!(
                    "{} checkpoint work failed: {error:?}",
                    self.nodes[index].name
                );
                self.fail(message);
                return Vec::new();
            }
        };
        if !actions.is_empty() {
            let summary = actions
                .iter()
                .map(|action| match action {
                    CheckpointAction::Sealed {
                        checkpoint,
                        sealed_digest,
                        participants,
                    } => format!(
                        "sealed {checkpoint} {} {participants:?}",
                        &sealed_digest[..8]
                    ),
                    _ => serde_json::to_value(action).unwrap()["action"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect::<Vec<_>>()
                .join(",");
            self.note(format!("{} checkpoint: {summary}", self.nodes[index].name));
        }
        if self.sabotage == Sabotage::ForgetTombstones
            && actions
                .iter()
                .any(|action| matches!(action, CheckpointAction::Trimmed { .. }))
        {
            self.nodes[index]
                .store
                .forget_tombstones_for_tests()
                .unwrap();
        }
        self.check_actions(index, &actions);
        self.check_index(index);
        self.tap(index);
        actions
    }

    fn write(&mut self, index: usize) {
        let input = if self.rng.chance(15) {
            self.person_claim(index)
        } else if self.rng.chance(65) {
            self.rule_claim(index)
        } else {
            self.any_claim(index)
        };
        let Some(input) = input else {
            return;
        };
        if let Ok(claim) = self.nodes[index].store.append_claim(&input) {
            *self.written.entry(claim.kind.clone()).or_default() += 1;
            // Observations a node keeps to itself never replicate, whoever wrote them.
            let replicated =
                st3_schema::registry()
                    .claim(&claim.kind)
                    .is_some_and(|spec| match spec.retention {
                        Retention::Durable => true,
                        Retention::SystemLocal => claim.actor.is_some(),
                        Retention::Local | Retention::Latest => false,
                    });
            if replicated
                && claim
                    .actor
                    .as_deref()
                    .is_some_and(|actor| actor.starts_with("person/"))
            {
                self.person_claims.insert(claim.id.clone());
            }
            self.check_index(index);
            self.tap(index);
        }
    }

    fn other_name(&mut self, index: usize) -> String {
        let others = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, node)| node.name.clone())
            .collect::<Vec<_>>();
        self.rng.pick(&others).clone()
    }

    /// A claim of a kind a checkpoint rule may drop, in the few slots a real fleet would fill.
    fn rule_claim(&mut self, index: usize) -> Option<ClaimInput> {
        let node = self.nodes[index].name.clone();
        let (subject, kind, actor, fields) = match self.rng.below(6) {
            0 => (
                format!("daemon/{node}"),
                "daemon.diagnostic",
                None,
                json!({
                    "severity": self.rng.pick(&["warning", "error"]),
                    "code": self.rng.pick(&["slow-request", "link-down", "disk-low"]),
                    "reason": self.fresh("reason"),
                }),
            ),
            1 => (
                format!("host/{}", self.other_name(index)),
                "transport.observed",
                None,
                json!({"status": self.rng.pick(&["up", "down"])}),
            ),
            2 => {
                let agent = format!(
                    "agent/run-{}/worker-{}",
                    self.rng.below(2),
                    self.rng.below(2)
                );
                let mut fields = json!({
                    "state": self.rng.pick(&["ready", "working", "idle", "blocked"]),
                    "incarnation_id": format!("inc-{}", self.rng.below(3)),
                    "observed_at_ms": self.serial,
                });
                for field in ["driver", "ask", "reason", "blocked_on"] {
                    if self.rng.chance(20) {
                        fields[field] = json!(self.fresh(field));
                    }
                }
                (agent.clone(), "harness.observed", Some(agent), fields)
            }
            3 => (
                format!("loop-run/gen-0/loop-{}", self.rng.below(2)),
                "loop.state",
                None,
                {
                    let mut fields = json!({
                        "status": self.rng.pick(&["running", "waiting", "done"]),
                        "round": self.rng.below(3),
                    });
                    if self.rng.chance(20) {
                        fields["items"] = json!([self.fresh("item")]);
                    }
                    fields
                },
            ),
            4 => (
                format!("observer/run-0/watch-{}", self.rng.below(2)),
                "observer.observed",
                None,
                json!({
                    "changed": self.rng.chance(50),
                    "cursor": self.fresh("cursor"),
                }),
            ),
            _ => {
                let subject = format!("subscription/run-0/sub-{}", self.rng.below(2));
                let request = format!("request-{}", self.rng.below(4));
                if self.rng.chance(20) {
                    (
                        subject,
                        "subscription.mission-started",
                        None,
                        json!({"request": request, "mission_run": format!("mission-run/{}", self.fresh("run"))}),
                    )
                } else {
                    (
                        subject,
                        "subscription.mission-deferred",
                        None,
                        json!({"request": request, "not_before_unix_ms": now_ms() + self.rng.below(100_000) as i64}),
                    )
                }
            }
        };
        Some(ClaimInput {
            subject,
            kind: kind.into(),
            actor,
            fields: fields
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(self.fresh("rule")),
        })
    }

    /// A claim a person writes. None may ever be dropped.
    fn person_claim(&mut self, _index: usize) -> Option<ClaimInput> {
        let (subject, kind, fields) = match self.rng.below(3) {
            0 => (
                format!("message/{}", self.fresh("message")),
                "message.sent",
                json!({"status": "sent", "title": self.fresh("title"), "content": "hello"}),
            ),
            1 => (
                format!("resource/{}", self.fresh("resource")),
                "resource.observed",
                json!({"state": {"value": self.serial}}),
            ),
            _ => (
                format!("attention/{}", self.fresh("attention")),
                "attention.requested",
                json!({
                    "reason": "a person asks",
                    "reviewer": PERSON,
                    "severity": "info",
                    "title": self.fresh("title"),
                }),
            ),
        };
        Some(ClaimInput {
            subject,
            kind: kind.into(),
            actor: Some(PERSON.into()),
            fields: fields
                .as_object()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(self.fresh("person")),
        })
    }

    /// A claim of any kind in the registry, with fields drawn from its schema. The store refuses
    /// many of them, as it would any client; the suite counts what it accepted.
    fn any_claim(&mut self, index: usize) -> Option<ClaimInput> {
        let registry = st3_schema::registry();
        let kinds = registry
            .claims
            .values()
            // Checkpoint claims come from checkpoint work. A planning preview without its
            // optional candidate revision stops the store's projection, so a restart fails;
            // only the planner writes those, always with one.
            .filter(|spec| {
                !spec.kind.starts_with("checkpoint.") && !spec.kind.starts_with("planning-session.")
            })
            .collect::<Vec<_>>();
        let spec = *self.rng.pick(&kinds);
        let family = self.rng.pick(&spec.subjects).clone();
        let subject = self.subject_of(&family, index)?;
        let mut fields = Map::new();
        for (name, field) in &spec.fields {
            if field.required || self.rng.chance(40) {
                fields.insert(name.clone(), self.value_of(field, index));
            }
        }
        let actor = match self.rng.below(4) {
            0 => None,
            1 => Some(PERSON.to_owned()),
            2 if subject.starts_with("agent/") => Some(subject.clone()),
            _ => Some("agent/run-0/worker-0".to_owned()),
        };
        Some(ClaimInput {
            subject,
            kind: spec.kind.clone(),
            actor,
            fields: fields.into_iter().collect(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(self.fresh("any")),
        })
    }

    fn subject_of(&mut self, family: &str, index: usize) -> Option<String> {
        let small = self.rng.below(3);
        Some(match family {
            "*" => format!("custom/convergence/{small}"),
            "account" => format!("account/example-{small}"),
            "agent" => format!("agent/run-{}/worker-{small}", self.rng.below(2)),
            "attention" => format!("attention/{}", self.fresh("attention")),
            "custom" => format!("custom/convergence/{small}"),
            "daemon" => format!("daemon/{}", self.nodes[index].name),
            "doc" => format!("doc/convergence/{small}"),
            "exec" => format!("exec/run-0/job-{small}"),
            "file" => format!("file/{}:/srv/example-{small}", self.nodes[index].name),
            "fleet-invite" => format!("fleet-invite/{}", self.fresh("invite")),
            "gate-operation" => format!("gate-operation/{}", self.fresh("gate")),
            "host" => format!("host/{}", self.other_name(index)),
            "loop-run" => format!("loop-run/gen-0/loop-{small}"),
            "message" => format!("message/{}", self.fresh("message")),
            "mission" => format!("mission/convergence-{small}"),
            "mission-run" => format!("mission-run/convergence-{small}"),
            "observer" => format!("observer/run-0/watch-{small}"),
            "person" => PERSON.to_owned(),
            "planning-session" => format!("planning-session/{}", self.fresh("session")),
            "pty" => format!("pty/run-0/term-{small}"),
            "repair" => format!("repair/{}", self.fresh("repair")),
            "resource" => format!("resource/convergence-{small}"),
            "revision-proposal" => format!("revision-proposal/{}", self.fresh("proposal")),
            "run-generation" => format!("run-generation/gen-{small}"),
            "schedule" => format!("schedule/run-0/tick-{small}"),
            "step-run" => format!("step-run/gen-0/step-{small}"),
            "subscription" => format!("subscription/run-0/sub-{small}"),
            _ => return None,
        })
    }

    fn value_of(&mut self, field: &FieldSpec, index: usize) -> Value {
        if !field.values.is_empty() {
            return json!(self.rng.pick(&field.values));
        }
        match field.value_type {
            ValueType::Boolean => json!(self.rng.chance(50)),
            ValueType::Integer => json!(self.rng.below(5)),
            ValueType::Number => json!(self.rng.below(100) as f64 / 10.0),
            ValueType::String | ValueType::Any => json!(self.fresh("text")),
            ValueType::Array => json!([]),
            ValueType::Object => json!({}),
            ValueType::SubjectReference => {
                let family = field
                    .reference_families
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "person".into());
                json!(
                    self.subject_of(&family, index)
                        .unwrap_or_else(|| PERSON.into())
                )
            }
        }
    }

    fn can_exchange(&self, from: usize, to: usize) -> bool {
        from != to
            && self.nodes[from].online
            && self.nodes[to].online
            && self.groups[from] == self.groups[to]
    }

    fn partitioned(&self) -> bool {
        self.groups.iter().any(|group| *group != self.groups[0])
    }

    fn run_schedule(&mut self, steps: usize) {
        for _ in 0..steps {
            let count = self.nodes.len();
            let index = self.rng.below(count);
            match self.rng.below(100) {
                0..=39 => {
                    if self.nodes[index].online {
                        self.write(index);
                    }
                }
                40..=64 => {
                    let to = self.rng.below(count);
                    if self.can_exchange(index, to) {
                        let received = self.exchange(index, to, false);
                        if received != 0 {
                            let (from, to) =
                                (self.nodes[index].name.clone(), self.nodes[to].name.clone());
                            self.note(format!("{from} -> {to}: {received}"));
                        }
                    }
                }
                65..=76 => {
                    if self.nodes[index].online && !self.nodes[index].old_build {
                        self.checkpoint_work(index, true);
                    }
                }
                77..=81 => {
                    self.days += 1;
                    for node in 0..count {
                        self.set_clock(node);
                    }
                    self.note(format!("day {}", self.days));
                }
                82..=84 => {
                    let groups = (0..count).map(|_| self.rng.below(2)).collect::<Vec<_>>();
                    self.groups = groups;
                    self.note(format!("partition {:?}", self.groups));
                }
                85..=87 => {
                    self.groups = vec![0; count];
                    self.note("heal".into());
                }
                88..=90 => {
                    self.nodes[index].online = !self.nodes[index].online;
                    let state = if self.nodes[index].online {
                        "online"
                    } else {
                        "offline"
                    };
                    self.note(format!("{} {state}", self.nodes[index].name));
                }
                91..=92 => {
                    self.note(format!("restart {}", self.nodes[index].name));
                    self.restart(index);
                }
                93..=95 => self.excuse(index),
                96 => {
                    if count < 5 {
                        let old =
                            !self.nodes.iter().any(|node| node.old_build) && self.rng.chance(30);
                        self.add_node(old);
                    }
                }
                _ => {
                    self.nodes[index].skew_ms =
                        (self.rng.below(6 * 24) as i64 - 3 * 24) * DAY_MS / 24;
                    self.set_clock(index);
                    self.note(format!(
                        "{} skew {}h",
                        self.nodes[index].name,
                        self.nodes[index].skew_ms / (DAY_MS / 24)
                    ));
                }
            }
        }
    }

    /// A person on an online node excuses a writer it cannot reach, or an old build.
    fn excuse(&mut self, index: usize) {
        if !self.nodes[index].online || self.nodes[index].old_build {
            return;
        }
        let candidates = (0..self.nodes.len())
            .filter(|other| {
                *other != index
                    && (self.nodes[*other].old_build
                        || !self.nodes[*other].online
                        || self.groups[*other] != self.groups[index])
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        let writer = self.nodes[*self.rng.pick(&candidates)].name.clone();
        let result = self.nodes[index]
            .store
            .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                writer: writer.clone(),
                reason: "unreachable in the simulation".into(),
                actor: PERSON.into(),
            });
        match result {
            Ok(claim) => {
                self.person_claims.insert(claim.id);
                if self.partitioned() {
                    self.excused_while_partitioned = true;
                }
                self.note(format!("{} excuses {writer}", self.nodes[index].name));
                self.check_index(index);
                self.tap(index);
            }
            Err(error) => {
                let message = format!(
                    "excusing {writer} failed: {}: {}",
                    error.code, error.message
                );
                self.fail(message);
            }
        }
    }

    /// Heal everything and bring every node back, then exchange and run checkpoint work until
    /// nothing moves.
    fn quiesce(&mut self) -> bool {
        let count = self.nodes.len();
        self.groups = vec![0; count];
        // Every machine comes back with a corrected clock. Checkpoints need participants to
        // agree on the newest due cut, so skewed clocks only delay them.
        for index in 0..count {
            self.nodes[index].online = true;
            self.nodes[index].skew_ms = 0;
            self.set_clock(index);
        }
        self.note("quiesce".into());
        // An old build holds every checkpoint up until a person excuses it, as the attention
        // request asks. Half the runs leave it waiting.
        if let Some(old) = self.nodes.iter().position(|node| node.old_build)
            && let Some(person) = self.nodes.iter().position(|node| !node.old_build)
            && self.rng.chance(50)
        {
            self.groups = vec![0; count];
            let writer = self.nodes[old].name.clone();
            let claim = self.nodes[person]
                .store
                .excuse_checkpoint_writer(&CheckpointExcuseRequest {
                    writer: writer.clone(),
                    reason: "an old build".into(),
                    actor: PERSON.into(),
                })
                .unwrap();
            self.person_claims.insert(claim.id);
            self.tap(person);
            self.note(format!("{} excuses {writer}", self.nodes[person].name));
        }
        for _ in 0..80 {
            let mut moved = false;
            for from in 0..count {
                for to in 0..count {
                    if from != to {
                        loop {
                            let received = self.exchange(from, to, true);
                            if received == 0 {
                                break;
                            }
                            moved = true;
                        }
                    }
                }
            }
            for index in 0..count {
                if self.nodes[index].old_build {
                    continue;
                }
                let actions = self.checkpoint_work(index, false);
                moved |= actions
                    .iter()
                    .any(|action| !matches!(action, CheckpointAction::ManifestNeeded { .. }));
            }
            if !moved {
                return true;
            }
        }
        false
    }

    fn check(&mut self) {
        let status = |store: &Store| store.replication_status(true, Some(FLEET), &[]).unwrap();
        let oracle = status(&self.oracle);
        let oracle_claims = claims(&self.oracle);
        let subjects = oracle_claims
            .values()
            .map(|claim| claim.subject.clone())
            .collect::<BTreeSet<_>>();
        // A fixed time, far enough ahead that every reader looks at every claim.
        let now = (now_ms() + (self.days + 10) * DAY_MS) as u128;
        let oracle_answers = self
            .oracle
            .checkpoint_reader_answers(&subjects, now)
            .unwrap();
        let first_new = self.nodes.iter().position(|node| !node.old_build);
        let reference = first_new.map(|index| {
            let store = &self.nodes[index].store;
            (
                status(store).authority_digest,
                store.trimmed_checkpoint().unwrap(),
            )
        });
        let reference_manifest = reference.as_ref().and_then(|(_, trimmed)| {
            trimmed.as_ref().map(|trimmed| {
                self.nodes[first_new.unwrap()]
                    .store
                    .checkpoint_manifest(&trimmed.id, trimmed.cut_unix_ms)
                    .unwrap()
            })
        });
        let mut failures = Vec::new();
        if oracle.invalid_records != 0 {
            failures.push(format!(
                "the oracle holds {} invalid records",
                oracle.invalid_records
            ));
        }
        for node in &self.nodes {
            let name = &node.name;
            let store = &node.store;
            let own = status(store);
            if own.invalid_records != 0 || own.pending_records != 0 {
                failures.push(format!(
                    "{name}: {} invalid and {} pending records",
                    own.invalid_records, own.pending_records
                ));
            }
            if own.graph_digest != oracle.graph_digest {
                failures.push(format!("{name}: graph digest differs from the oracle's"));
            }
            let answers = store.checkpoint_reader_answers(&subjects, now).unwrap();
            for (subject, answer) in &oracle_answers {
                if answers.get(subject) != Some(answer) {
                    failures.push(format!(
                        "{name}: readers answer differently for {subject}: {} vs oracle {answer}",
                        answers.get(subject).map_or("nothing", String::as_str)
                    ));
                }
            }
            let held = claims(store);
            let trimmed = store.trimmed_checkpoint().unwrap();
            let manifest = trimmed.as_ref().map(|trimmed| {
                store
                    .checkpoint_manifest(&trimmed.id, trimmed.cut_unix_ms)
                    .unwrap()
            });
            let tombstones = manifest
                .as_ref()
                .map(|manifest| {
                    manifest
                        .claims
                        .iter()
                        .map(|claim| claim.id.clone())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            for (id, claim) in &oracle_claims {
                let droppable = RULE_KINDS.contains(&claim.kind.as_str())
                    && !claim
                        .actor
                        .as_deref()
                        .is_some_and(|actor| actor.starts_with("person/"));
                match held.get(id) {
                    Some(own) if own.body != claim.body || own.kind != claim.kind => {
                        failures.push(format!("{name}: {id} has another body"));
                    }
                    Some(_) => {
                        if tombstones.contains(id) {
                            failures.push(format!("{name}: holds {id}, which it tombstoned"));
                        }
                    }
                    None if !droppable || self.person_claims.contains(id) => {
                        failures.push(format!(
                            "{name}: lacks {id} ({} on {}), which no rule drops",
                            claim.kind, claim.subject
                        ));
                    }
                    None if !tombstones.contains(id) && !self.excused_while_partitioned => {
                        failures.push(format!(
                            "{name}: lacks {id} ({} on {}) without a tombstone",
                            claim.kind, claim.subject
                        ));
                    }
                    None => {}
                }
            }
            for id in held.keys() {
                if !oracle_claims.contains_key(id) {
                    failures.push(format!("{name}: holds {id}, which the oracle never saw"));
                }
            }
            if node.old_build {
                continue;
            }
            if let Some((authority, reference_trimmed)) = &reference {
                if &own.authority_digest != authority {
                    failures.push(format!("{name}: authority digest differs"));
                }
                if &trimmed != reference_trimmed {
                    failures.push(format!(
                        "{name}: trimmed {trimmed:?}, not {reference_trimmed:?}"
                    ));
                }
                if manifest != reference_manifest {
                    failures.push(format!("{name}: tombstones differ"));
                }
            }
            if store.checkpoint_manifest_need().unwrap().is_some() {
                failures.push(format!("{name}: still needs a manifest after quiescence"));
            }
            let checkpoint_claims = store.checkpoint_claims().unwrap();
            for (cut, certified) in stable_checkpoints(&checkpoint_claims) {
                if certified.len() > 1 && !self.excused_while_partitioned {
                    failures.push(format!(
                        "{name}: {} has {} certificates without an excused partition",
                        checkpoint_name(cut),
                        certified.len()
                    ));
                }
                assert_eq!(
                    certificates(&checkpoint_claims, &checkpoint_name(cut)).len(),
                    certified.len()
                );
            }
        }
        self.failures.extend(failures);
    }

    fn report(&self) -> String {
        format!(
            "seed {} failed ({} nodes, sabotage {:?}):\n  {}\nschedule:\n  {}",
            self.seed,
            self.nodes.len(),
            self.sabotage,
            self.failures.join("\n  "),
            self.schedule.join("\n  ")
        )
    }
}

/// What the daemon does after a receipt: admit, repair and project.
fn admit(store: &Store) -> anyhow::Result<()> {
    store.validate_replication_backlog()?;
    store.apply_replication_repairs()?;
    store.project_replication_backlog()?;
    Ok(())
}

fn claims(store: &Store) -> BTreeMap<String, ClaimRecord> {
    let mut all = BTreeMap::new();
    let mut after = 0;
    loop {
        let page = store
            .claims_page(None, None, after, None, false, 1_000)
            .unwrap();
        let Some(last) = page.claims.last() else {
            return all;
        };
        after = last.store_index;
        for claim in page.claims {
            all.insert(claim.id.clone(), claim);
        }
    }
}

struct Outcome {
    failure: Option<String>,
    written: BTreeMap<String, usize>,
    trimmed: bool,
}

fn run(seed: u64, sabotage: Sabotage) -> Outcome {
    let mut world = World::new(seed, sabotage);
    let steps = 300 + world.rng.below(300);
    world.run_schedule(steps);
    if !world.quiesce() {
        world.fail("the nodes did not stop moving".into());
    }
    world.check();
    let trimmed = world
        .nodes
        .iter()
        .any(|node| node.store.trimmed_checkpoint().unwrap().is_some());
    Outcome {
        failure: (!world.failures.is_empty()).then(|| world.report()),
        written: world.written,
        trimmed,
    }
}

fn run_seeds(seeds: impl IntoIterator<Item = u64>) {
    let mut failures = Vec::new();
    let mut written = BTreeMap::<String, usize>::new();
    let mut trimmed = 0;
    let mut runs = 0;
    for seed in seeds {
        let outcome = run(seed, Sabotage::None);
        runs += 1;
        trimmed += usize::from(outcome.trimmed);
        for (kind, count) in outcome.written {
            *written.entry(kind).or_default() += count;
        }
        if let Some(failure) = outcome.failure {
            eprintln!("{failure}");
            failures.push(seed);
        }
    }
    eprintln!(
        "{runs} runs, {trimmed} trimmed; wrote {} kinds: {written:?}",
        written.len()
    );
    assert!(failures.is_empty(), "failing seeds: {failures:?}");
    // The suite is worth something only if checkpoints trimmed in most runs.
    assert!(trimmed * 2 >= runs, "only {trimmed} of {runs} runs trimmed");
}

#[test]
fn every_seed_converges() {
    run_seeds(SEEDS.iter().copied());
}

/// Each deliberate bug must make at least one seed fail, or the checks cannot see it.
#[test]
fn the_checks_catch_each_deliberate_bug() {
    for sabotage in [Sabotage::ForgetTombstones, Sabotage::PartialAdoption] {
        let caught = SEEDS
            .iter()
            .find(|seed| run(**seed, sabotage).failure.is_some());
        assert!(caught.is_some(), "no seed caught {sabotage:?}");
    }
}

#[test]
#[ignore = "set CONVERGENCE_SEED"]
fn one_seed() {
    let seed = std::env::var("CONVERGENCE_SEED")
        .expect("CONVERGENCE_SEED names the seed")
        .parse()
        .unwrap();
    run_seeds([seed]);
}

#[test]
#[ignore = "explores new seeds; set CONVERGENCE_RUNS"]
fn explore() {
    let runs: u64 = std::env::var("CONVERGENCE_RUNS")
        .ok()
        .and_then(|runs| runs.parse().ok())
        .unwrap_or(100);
    let start = now_ms() as u64;
    run_seeds((0..runs).map(|offset| start.wrapping_add(offset)));
}
