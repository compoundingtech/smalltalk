//! Actual Kernel namespace row parity against the complete public HTTP agent-card oracle.
//!
//! These controls apply the real Kernel to actual indexed physical extraction and committed
//! OLD/new captures, then read prospective rows without constructing a Root or source cut.
//! They establish row parity only: no namespace publication, provider activation or native
//! source/producer certification is performed.
use crate::{
    api::AppState,
    model::{ClaimInput, MissionRunRequest},
    store::Store,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use smallclaims::ivm::Views;
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

const AGENT: &str = "agent/node.alpha";
const PUBLIC_FIELDS: &[&str] = &[
    "id",
    "kind",
    "revision",
    "updated_at",
    "name",
    "state",
    "reachability",
    "runtime_ids",
    "owner_run_id",
    "driver",
    "harness_state",
    "harness_error_state",
    "since",
    "blocked_on",
    "ask",
    "reason",
    "host_id",
    "workspace",
    "checkout",
    "last_activity_at",
    "silent_since",
    "fault",
    "incarnation_id",
    "current_session_id",
    "current_work_ids",
    "active_work_count",
    "next_work_id",
    "upcoming_work_ids",
    "queued_work_count",
    "current_work",
    "next_work",
    "upcoming_work",
    "usage",
    "under",
    "operational",
    "suspension",
    "handoff",
    "rollout",
    "todo",
    "observation",
    "subagents",
];

struct Fixture {
    state: AppState,
}
#[derive(Debug)]
struct PublicCut {
    store_index: u64,
    local_position: u64,
    snapshot: Value,
    cards: Vec<Value>,
}
impl Fixture {
    fn with_store(root: &Path, store: Arc<Store>) -> Self {
        Self {
            state: AppState {
                node: store.origin().to_owned(),
                store,
                notify: Arc::new(Notify::new()),
                event_notify: watch::channel(0).0,
                state_dir: root.into(),
                pty_root: root.join("pty"),
                pty_binary: "pty".into(),
                fleet_id: None,
                configured_peers: vec![],
                client_relay: None,
                native_session_home: None,
                planner_default: Default::default(),
            },
        }
    }
    fn new(root: &Path) -> Self {
        Self::with_store(
            root,
            Arc::new(
                Store::open_with_ivm_views(
                    &root.join("oracle.sqlite"),
                    "node",
                    Arc::new(smallclaims::ivm::Views::new(super::cards::definitions()).unwrap()),
                )
                .unwrap(),
            ),
        )
    }
    fn store(&self) -> &Store {
        &self.state.store
    }
    fn populate(&self) {
        let intent=crate::graph::parse_intent("version 2\nagent \"alpha\" { name \"Alpha\"; command \"true\"; }\nagent \"beta\" { name \"Beta\"; command \"true\"; }\nmission \"card-fixture\" state=\"ready\" { concurrent-runs max=4; goal \"Prepare a sample\"; step \"build\" { assigned-to \"agent/node.alpha\"; title \"Build sample\"; goal \"Produce sample\"; } }\n","node").unwrap();
        self.store()
            .apply_internal(&intent, "declare-card-fixture")
            .unwrap();
        self.append(
            "runtime.observed",
            json!({"status":"running","runtime_id":"runtime-one","incarnation_id":"one"}),
        );
        // Invented nonnative driver keeps the oracle independent of the process-global live
        // delivery producer. Local native delivery refusal belongs to the actual provider controls.
        self.append(
            "harness.observed",
            json!({"state":"working","driver":"invented","incarnation_id":"one"}),
        );
        self.append("harness.session-file",json!({"harness":"omp","agent":AGENT,"session_id":"native-one","path":"/tmp/card-fixture-session"}));
        self.append("harness.todo.observed",json!({"harness":"omp","session_id":"native-one","incarnation_id":"one","observed_at":"2026-10-01T09:00:00Z","source_op":"update","phases":[{"name":"Build","tasks":[{"content":"Sample","status":"blocked","blocker":"Approval"}]}],"totals":{"pending":0,"in_progress":0,"completed":0,"blocked":1,"abandoned":0},"truncated":false}));
        let lease = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 600_000;
        self.append("subagent.appeared",json!({"subagent_id":"helper-one","subagent_type":"Explore","description":"Inspect sample","driver":"claude","session_id":"helper-session","incarnation_id":"one","started_at_unix_ms":1,"lease_expires_at_unix_ms":lease}));
        self.append(
            "runtime.reconcile-decision",
            json!({"key":"member-reconcile","decision":"member-fault","reason":"fixture fault"}),
        );
        for n in 0..2 {
            let run = self
                .store()
                .create_mission_run(&MissionRunRequest {
                    mission: "card-fixture".into(),
                    revision: None,
                    workspace: "/tmp/card-fixture".into(),
                    requester: Some("person/avery".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("card-run-{n}"),
                })
                .unwrap();
            for step in run.steps {
                self.store()
                    .set_step_state(&step.subject, "ready", None)
                    .unwrap();
            }
        }
        self.activity("A message establishes activity");
    }
    fn append(&self, kind: &str, fields: Value) -> crate::model::ClaimRecord {
        self.store()
            .append_claim(&ClaimInput {
                subject: AGENT.into(),
                kind: kind.into(),
                actor: Some(AGENT.into()),
                fields: serde_json::from_value(fields).unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
    }
    fn activity(&self, content: &str) {
        self.store()
            .append_claim(&ClaimInput {
                subject: format!(
                    "message/{}",
                    hex::encode(sha2::Sha256::digest(content.as_bytes()))
                ),
                kind: "message.sent".into(),
                actor: Some(AGENT.into()),
                fields: serde_json::from_value(
                    json!({"from":AGENT,"to":"person/avery","content":content,"status":"sent"}),
                )
                .unwrap(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    fn local_position(&self) -> u64 {
        self.store()
            .readers
            .get()
            .query_row(
                "SELECT COALESCE(MAX(id),0) FROM local_observations",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    async fn request(&self, path: &str, actor: &str) -> (StatusCode, Value) {
        let response = crate::api::router(self.state.clone())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("x-st3-person", actor)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
    async fn full_cut(&self, actor: &str) -> PublicCut {
        let index = self.store().index().unwrap();
        let local = self.local_position();
        let (status, response) = self.request("/v1/client/agents?limit=200", actor).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(
            self.store().index().unwrap(),
            index,
            "oracle source changed during read"
        );
        assert_eq!(
            self.local_position(),
            local,
            "oracle local source changed during read"
        );
        assert_eq!(response["snapshot"]["store_index"], index, "{response}");
        let page = &response["value"];
        assert!(
            page["next_cursor"].is_null(),
            "fixture must fit one complete page: {page}"
        );
        let cards = page["items"].as_array().unwrap().clone();
        for row in &cards {
            for field in PUBLIC_FIELDS {
                assert!(
                    row.get(*field).is_some(),
                    "missing full public field {field}: {row}"
                );
            }
            assert!(row.get("_status_source").is_none());
        }
        assert!(
            cards
                .windows(2)
                .all(|pair| (pair[0]["name"].as_str(), pair[0]["id"].as_str())
                    <= (pair[1]["name"].as_str(), pair[1]["id"].as_str()))
        );
        PublicCut {
            store_index: index,
            local_position: local,
            snapshot: response["snapshot"].clone(),
            cards,
        }
    }
}
use sha2::Digest as _;
fn card(cut: &PublicCut) -> &Value {
    cut.cards.iter().find(|row| row["id"] == AGENT).unwrap()
}

fn future_content(f: &KernelFixture, advance: u64) -> (u64, u128) {
    let position = f.store().index().unwrap();
    let record = f.oracle.append(
        "harness.timeline",
        json!({
            "operation":"append", "entry_id":"future-card-content", "revision":1,
            "role":"assistant", "entry_type":"content", "final":true,
            "body":{"media_type":"text/plain","text":"Future anchored content"},
            "driver":"claude", "incarnation_id":"one", "sequence":1
        }),
    );
    assert_eq!(f.store().index().unwrap(), position);
    let anchor = position + advance;
    f.store().connection.batched(|tx| {
        let id: u64 = tx.query_row(
            "SELECT id FROM local_observations WHERE subject=?1 AND kind='harness.timeline' ORDER BY id DESC LIMIT 1",
            [AGENT], |row| row.get(0),
        )?;
        tx.execute("UPDATE local_observations SET after_store_index=?1 WHERE id=?2",
            rusqlite::params![anchor, id])?;
        Ok::<_, anyhow::Error>(())
    }).unwrap().unwrap();
    (anchor, record.accepted_at_unix_ms)
}

#[tokio::test]
async fn actual_kernel_future_anchored_local_content_stays_outside_full_public_cut() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let before = f.compare("person/avery").await;
    let replacements = f.replacements.load(Ordering::SeqCst);
    let (anchor, _) = future_content(&f, 2);
    assert!(anchor > f.store().index().unwrap());
    let after = f.compare("person/avery").await;
    assert_eq!(
        card(&after)["last_activity_at"],
        card(&before)["last_activity_at"]
    );
    assert!(f.replacements.load(Ordering::SeqCst) > replacements);
}

#[tokio::test]
async fn actual_kernel_normal_claim_advance_includes_local_content_at_exact_anchor() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let (anchor, observed) = future_content(&f, 2);
    f.compare("person/avery").await;
    f.oracle.append(
        "runtime.reconcile-decision",
        json!({
            "key":"other-cut-advance", "decision":"noop", "reason":"Below activity anchor"
        }),
    );
    assert_eq!(f.store().index().unwrap(), anchor - 1);
    f.compare("person/avery").await;
    f.oracle.append(
        "runtime.reconcile-decision",
        json!({
            "key":"other-cut-advance", "decision":"noop", "reason":"At activity anchor"
        }),
    );
    assert_eq!(f.store().index().unwrap(), anchor);
    let after = f.compare("person/avery").await;
    assert_eq!(
        card(&after)["last_activity_at"],
        json!(crate::api::client_timestamp(observed))
    );
}

use super::{agent_card_source, cards};
use crate::store::collection_ivm::{agent_source, scope};
use anyhow::Result;
use rusqlite::{Connection, Transaction};
use smallclaims::ivm::install::{Installer, Limits, Mutation, Namespace, Operator};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

// Trace the context assigned to the actual Kernel by actual Installer extraction. All
// Operator behavior, including refusal to publish without coverage, is forwarded unchanged.
struct TracedKernel {
    kernel: Arc<agent_card_source::Kernel>,
    namespace: Arc<Mutex<Option<Namespace>>>,
}
impl Operator for TracedKernel {
    fn name(&self) -> &'static str {
        self.kernel.name()
    }
    fn fingerprint(&self) -> &'static str {
        self.kernel.fingerprint()
    }
    fn source(&self) -> &'static str {
        self.kernel.source()
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        self.kernel.create_schema(c)
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        *self.namespace.lock().unwrap() = Some(ns.clone());
        self.kernel.apply(tx, ns, rows)
    }
    fn validate_publication(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
        self.kernel.validate_publication(tx, ns)
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        self.kernel.reclaim(tx, ns, rows)
    }
}
struct KernelFixture {
    oracle: Fixture,
    kernel: Arc<agent_card_source::Kernel>,
    installer: Arc<Installer>,
    namespace: Namespace,
    job: String,
    captured: Arc<AtomicUsize>,
    replacements: Arc<AtomicUsize>,
}
impl KernelFixture {
    fn attach(oracle: Fixture) -> Self {
        let store = oracle.state.store.clone();
        let views = store.ivm_views().unwrap();
        let kernel = Arc::new(agent_card_source::Kernel::new(store.origin()));
        let namespace = Arc::new(Mutex::new(None));
        let installer = Arc::new(
            Installer::new(vec![Box::new(TracedKernel {
                kernel: kernel.clone(),
                namespace: namespace.clone(),
            })])
            .unwrap(),
        );
        store
            .connection
            .batched(|tx| -> Result<()> {
                installer.create_schema(tx)?;
                agent_source::install_capture_for(tx, store.origin(), 1)?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let captured = Arc::new(AtomicUsize::new(0));
        let replacements = Arc::new(AtomicUsize::new(0));
        let finishing = (
            views.clone(),
            installer.clone(),
            kernel.clone(),
            namespace.clone(),
            captured.clone(),
            replacements.clone(),
        );
        store
            .install_transaction_hooks(scope::prepare, move |tx| {
                scope::finalize(tx, &finishing.0, &finishing.1, cards::SOURCE, |tx, page| {
                    let rows: Vec<_> = page
                        .iter()
                        .flat_map(|change| change.replacements.iter().cloned())
                        .collect();
                    finishing.4.fetch_add(rows.len(), Ordering::SeqCst);
                    finishing.5.fetch_add(
                        rows.iter().filter(|row| row.old.is_some()).count(),
                        Ordering::SeqCst,
                    );
                    if let Some(ns) = finishing.3.lock().unwrap().clone() {
                        for rows in rows.chunks(128) {
                            finishing.2.apply(tx, &ns, rows)?;
                        }
                    }
                    Ok(scope::Coverage::Complete) // SQL capture completion only, never a card/source certificate.
                })?;
                Ok(())
            })
            .unwrap();
        store
            .connection
            .batched(|tx| {
                scope::begin_install(
                    tx,
                    &views,
                    &installer,
                    cards::SOURCE,
                    &agent_source::capture_fingerprint_for(store.origin())?,
                    1,
                )
            })
            .unwrap()
            .unwrap();
        let at = crate::api::client_now_ms();
        store
            .connection
            .batched(|tx| agent_source::clock::tick(tx, at, agent_source::clock::Reason::Kernel))
            .unwrap()
            .unwrap();
        let job = store
            .connection
            .batched(|tx| {
                installer.start(
                    tx,
                    cards::VIEW,
                    Limits {
                        page_rows: 128,
                        page_bytes: 1024 * 1024,
                        pending_rows: 4096,
                        pending_bytes: 16 * 1024 * 1024,
                        total_rows: 100_000,
                        callback_ms: 1000,
                        lifetime_ms: 60_000,
                    },
                    0,
                )
            })
            .unwrap()
            .unwrap();
        for _ in 0..64 {
            if installer
                .progress(&store.readers.get(), &job)
                .unwrap()
                .phase
                != "scan"
            {
                break;
            }
            let c = store.readers.get();
            c.execute_batch("BEGIN DEFERRED").unwrap();
            let page = agent_source::extract::scan_page_for(
                &c,
                &installer,
                &job,
                store.origin(),
                128,
                1024 * 1024,
            )
            .unwrap();
            c.execute_batch("COMMIT").unwrap();
            drop(c);
            let outcome = store
                .connection
                .batched(|tx| installer.scan(tx, &page, 0))
                .unwrap()
                .unwrap();
            assert!(
                matches!(outcome, smallclaims::ivm::install::Outcome::Progress),
                "{outcome:?}"
            );
        }
        assert_eq!(
            installer
                .progress(&store.readers.get(), &job)
                .unwrap()
                .phase,
            "catchup"
        );
        let namespace = namespace.lock().unwrap().clone().unwrap();
        let result = Self {
            oracle,
            kernel,
            installer,
            namespace,
            job,
            captured,
            replacements,
        };
        result.drain();
        result
    }
    fn store(&self) -> &Store {
        self.oracle.store()
    }
    fn drain(&self) {
        for _ in 0..128 {
            self.store()
                .connection
                .batched(|tx| self.kernel.apply(tx, &self.namespace, &[]))
                .unwrap()
                .unwrap();
            if agent_card_source::footprint(
                &self.store().readers.get(),
                &self.namespace,
                self.store().origin(),
            )
            .is_ok()
            {
                return;
            }
        }
        let c = self.store().readers.get();
        let work=c.prepare("SELECT kind,key FROM local_agent_card_source_work WHERE namespace=?1 ORDER BY kind,key").unwrap().query_map([self.namespace.as_str()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        panic!("actual Kernel did not close prospective rows: {work:?}");
    }
    fn rows(&self, frame_time: &str) -> Vec<Value> {
        let c = self.store().readers.get();
        let window = cards::ranked_window(&c, &self.namespace, 200, false, None).unwrap();
        assert!(!window.has_more);
        window
            .keys
            .iter()
            .map(|key| {
                let mut row = cards::row(&c, &self.namespace, key).unwrap().unwrap();
                if key.request_time {
                    row["updated_at"] = json!(frame_time);
                }
                row
            })
            .collect()
    }
    fn unpublished(&self) {
        let c = self.store().readers.get();
        assert!(self.installer.root(&c, cards::VIEW).is_err());
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM local_agent_card_coverage WHERE namespace=?1",
                [self.namespace.as_str()],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM local_agent_source_boundary WHERE namespace=?1",
                [self.namespace.as_str()],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
        assert!(scope::readable(&c).unwrap());
        assert_eq!(
            self.installer.progress(&c, &self.job).unwrap().phase,
            "catchup"
        );
    }
    async fn compare(&self, actor: &str) -> PublicCut {
        self.store()
            .connection
            .batched(|tx| {
                agent_source::clock::tick(
                    tx,
                    crate::api::client_now_ms(),
                    agent_source::clock::Reason::Kernel,
                )
            })
            .unwrap()
            .unwrap();
        self.drain();
        let before = self
            .installer
            .position(&self.store().readers.get(), cards::SOURCE)
            .unwrap();
        let expected = self.oracle.full_cut(actor).await;
        let rows = self.rows(expected.snapshot["created_at"].as_str().unwrap());
        assert_eq!(
            self.installer
                .position(&self.store().readers.get(), cards::SOURCE)
                .unwrap(),
            before
        );
        let differences: Vec<_> = rows
            .iter()
            .zip(&expected.cards)
            .filter_map(|(actual, public)| {
                if actual == public {
                    return None;
                }
                let keys: std::collections::BTreeSet<_> = actual
                    .as_object()
                    .unwrap()
                    .keys()
                    .chain(public.as_object().unwrap().keys())
                    .collect();
                let fields: BTreeMap<_, _> = keys
                    .into_iter()
                    .filter(|key| actual.get(*key) != public.get(*key))
                    .map(|key| {
                        (
                            key,
                            json!({"kernel":actual.get(key),"public":public.get(key)}),
                        )
                    })
                    .collect();
                Some(json!({"id":public["id"], "fields":fields}))
            })
            .collect();
        assert!(
            rows == expected.cards,
            "actual Kernel/public full-field mismatch: {} versus {} rows; {}",
            rows.len(),
            expected.cards.len(),
            serde_json::to_string(&differences).unwrap()
        );
        self.unpublished();
        expected
    }
}

#[tokio::test]
async fn actual_kernel_populated_full_public_queue_labels_aux_and_activity_parity() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let cut = f.compare("person/avery").await;
    assert_eq!(card(&cut)["queued_work_count"], 2);
    assert_eq!(card(&cut)["next_work"]["title"], "Build sample");
    assert_eq!(card(&cut)["todo"]["snapshot"]["totals"]["blocked"], 1);
    assert_eq!(card(&cut)["subagents"][0]["id"], "helper-one");
    assert_eq!(card(&cut)["fault"], "fixture fault");
    assert!(card(&cut)["last_activity_at"].is_string());
    assert!(f.captured.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn actual_kernel_committed_same_index_heartbeat_and_activity_preserve_full_parity() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let before = f.compare("person/avery").await;
    let captured = f.captured.load(Ordering::SeqCst);
    let local=f.oracle.append("harness.observed",json!({"state":"working","driver":"invented","incarnation_id":"one","observed_at_ms":crate::api::client_now_ms() as u64}));
    assert!(crate::store::local_observation_position(&local).is_some());
    let heartbeat = f.compare("person/avery").await;
    assert_eq!(heartbeat.store_index, before.store_index);
    assert!(heartbeat.local_position > before.local_position);
    assert!(f.captured.load(Ordering::SeqCst) > captured);
    f.oracle.activity("A later actual captured activity source");
    let activity = f.compare("person/avery").await;
    assert!(activity.store_index > heartbeat.store_index);
}

#[tokio::test]
async fn actual_kernel_managed_rollback_preserves_rows_capture_and_source_position() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let before = f.compare("person/avery").await;
    let position = f
        .installer
        .position(&f.store().readers.get(), cards::SOURCE)
        .unwrap();
    let capture = f.captured.load(Ordering::SeqCst);
    let fault = f
        .store()
        .latest_claim(AGENT, Some("runtime.reconcile-decision"))
        .unwrap()
        .unwrap();
    let result=f.store().connection.batched(|tx|->Result<()> {
        tx.execute("UPDATE claims SET body=json_set(body,'$.fields.decision','member-started') WHERE id=?1",[fault.id])?;
        tx.execute("UPDATE local_observations SET body=json_set(body,'$.fields.ask','rolled back') WHERE subject=?1 AND kind='harness.observed'",[AGENT])?;
        anyhow::bail!("actual managed source replacement rollback")
    }).unwrap();
    assert!(result.is_err());
    assert_eq!(f.captured.load(Ordering::SeqCst), capture);
    assert_eq!(
        f.installer
            .position(&f.store().readers.get(), cards::SOURCE)
            .unwrap(),
        position
    );
    assert_eq!(f.compare("person/avery").await.cards, before.cards);
}

#[tokio::test]
async fn actual_kernel_public_actor_boundary_matches_all_rows_and_refuses_invalid_identity() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    assert_eq!(
        f.compare("person/avery").await.cards,
        f.compare("person/ada").await.cards
    );
    let (status, _) = f
        .oracle
        .request("/v1/client/agents?limit=200", "resource/not-an-actor")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn actual_kernel_committed_queue_labels_todo_subagent_and_arrival_fault_replacements() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let before = f.compare("person/avery").await;
    let captures = f.captured.load(Ordering::SeqCst);
    let replacements = f.replacements.load(Ordering::SeqCst);
    let next = card(&before)["next_work_id"].as_str().unwrap().to_owned();
    f.store().set_step_state(&next, "completed", None).unwrap();
    let queue = f.compare("person/avery").await;
    assert_eq!(card(&queue)["queued_work_count"], 1);
    assert_ne!(card(&queue)["next_work_id"], next);
    assert_eq!(card(&queue)["next_work"]["title"], "Build sample");
    f.oracle.append("harness.todo.observed",json!({"harness":"omp","session_id":"native-one","incarnation_id":"one","observed_at":"2026-10-08T00:00:00Z","source_op":"update","phases":[{"name":"Build","tasks":[{"content":"Sample","status":"completed"}]}],"totals":{"pending":0,"in_progress":0,"completed":1,"blocked":0,"abandoned":0},"truncated":false}));
    let todo = f.compare("person/avery").await;
    assert_eq!(card(&todo)["todo"]["snapshot"]["totals"]["completed"], 1);
    f.oracle.append(
        "subagent.ended",
        json!({"subagent_id":"helper-one","outcome":"completed"}),
    );
    let ended = f.compare("person/avery").await;
    assert_eq!(card(&ended)["subagents"], json!([]));
    f.oracle.append(
        "runtime.reconcile-decision",
        json!({"key":"member-reconcile","decision":"member-started"}),
    );
    let repaired = f.compare("person/avery").await;
    assert!(card(&repaired)["fault"].is_null());
    assert!(f.captured.load(Ordering::SeqCst) > captures);
    assert!(
        f.replacements.load(Ordering::SeqCst) > replacements,
        "actual OLD/new replacement capture required"
    );
}

#[tokio::test]
async fn actual_kernel_reverse_signed_replication_envelopes_match_full_public_rows() {
    let root = tempfile::tempdir().unwrap();
    let origin = Fixture::new(root.path());
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let key = Arc::new(crate::fleet::MemberKey::generate().unwrap().0);
    origin.store().bind_fleet(fleet).unwrap();
    origin.store().pin_fleet_anchor(key.public()).unwrap();
    origin.store().set_member_key(Some(key.clone())).unwrap();
    origin.populate();
    let views = Arc::new(Views::new(cards::definitions()).unwrap());
    let store = Arc::new(
        Store::open_with_ivm_views(&root.path().join("peer.sqlite"), "peer", views).unwrap(),
    );
    store.bind_fleet(fleet).unwrap();
    store.pin_fleet_anchor(key.public()).unwrap();
    store
        .set_member_key(Some(Arc::new(
            crate::fleet::MemberKey::generate().unwrap().0,
        )))
        .unwrap();
    let target = KernelFixture::attach(Fixture::with_store(root.path(), store));
    let before = target.compare("person/avery").await;
    assert!(before.cards.is_empty());
    let mut exchange = origin
        .store()
        .export_replication_exchange_answering(
            fleet,
            &target.store().replication_inventory().unwrap(),
            &target.store().replication_signature_requests().unwrap(),
        )
        .unwrap();
    let original: Vec<_> = exchange
        .envelopes
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert!(original.len() > 2);
    exchange.envelopes.reverse();
    let reversed = exchange.envelopes.clone();
    assert_ne!(
        original,
        reversed
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect::<Vec<_>>()
    );
    // Only delivery order changes; every authenticated envelope and its signature is intact.
    // One transport envelope per transaction keeps the actual capture work bounded.
    for envelope in reversed {
        let mut page = exchange.clone();
        page.envelopes = vec![envelope];
        target
            .store()
            .receive_replication_exchange("node", fleet, &page)
            .unwrap();
        target.store().validate_replication_backlog().unwrap();
    }
    let admission = target.store().validate_replication_backlog().unwrap();
    assert_eq!((admission.invalid, admission.unknown), (0, 0));
    target.store().project_replication_backlog().unwrap();
    let after = target.compare("person/avery").await;
    assert_eq!(card(&after)["queued_work_count"], 2);
    assert_eq!(card(&after)["todo"]["snapshot"]["totals"]["blocked"], 1);
    assert_eq!(card(&after)["subagents"][0]["id"], "helper-one");
    assert_eq!(
        after.local_position, 0,
        "peer must not manufacture source local log"
    );
    assert!(target.replacements.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn actual_kernel_prospective_namespace_rows_survive_store_reopen_without_publication() {
    let root = tempfile::tempdir().unwrap();
    let oracle = Fixture::new(root.path());
    oracle.populate();
    let f = KernelFixture::attach(oracle);
    let before = f.compare("person/avery").await;
    let ns = f.namespace.clone();
    let installer = f.installer.clone();
    let job = f.job.clone();
    let position = installer
        .position(&f.store().readers.get(), cards::SOURCE)
        .unwrap();
    drop(f);
    let views = Arc::new(Views::new(cards::definitions()).unwrap());
    let store = Arc::new(
        Store::open_with_ivm_views(&root.path().join("oracle.sqlite"), "node", views).unwrap(),
    );
    let oracle = Fixture::with_store(root.path(), store);
    let after = oracle.full_cut("person/avery").await;
    let c = oracle.store().readers.get();
    assert_eq!(installer.position(&c, cards::SOURCE).unwrap(), position);
    assert!(
        scope::readable(&c).unwrap(),
        "reopen must not silently bypass captured inputs"
    );
    let window = cards::ranked_window(&c, &ns, 200, false, None).unwrap();
    assert!(!window.has_more);
    let rows: Vec<_> = window
        .keys
        .iter()
        .map(|key| {
            let mut row = cards::row(&c, &ns, key).unwrap().unwrap();
            if key.request_time {
                row["updated_at"] = after.snapshot["created_at"].clone();
            }
            row
        })
        .collect();
    assert_eq!(rows, after.cards);
    assert_eq!(before.cards, after.cards);
    assert_eq!(before.store_index, after.store_index);
    assert_eq!(before.local_position, after.local_position);
    assert!(installer.root(&c, cards::VIEW).is_err());
    assert_eq!(installer.progress(&c, &job).unwrap().phase, "catchup");
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM local_agent_card_coverage WHERE namespace=?1",
            [ns.as_str()],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}
