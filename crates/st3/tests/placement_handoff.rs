#![cfg(unix)]
use serde_json::Value;
use st3::api::AppState;
use st3::model::MemberSpec;
use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
use st3::store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, watch};
const FLEET: &str = "7d3f9a2e-5b6c-4e1d-8a0f-2c9b8e7d6f5a";
const SEAT: &str = "agent/example/mover";

async fn override_sources(
    node: &Node,
    token: &str,
    sources: &[&str],
    key: &str,
) -> anyhow::Result<Value> {
    st3::client::Client::unix(node.root.path().join("st3.sock"))
        .post(
            "/v1/agents/source-offline",
            &serde_json::json!({
                "subject": SEAT, "actor": "person/avery", "desired_token": token,
                "sources": sources, "idempotency_key": key,
            }),
        )
        .await
}

#[tokio::test]
async fn source_offline_cli_records_the_actor_and_returning_source_still_stops() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    // The source takes no passes or replication exchanges while the move is requested.
    let result = cobalt
        .cli(&[
            "agents",
            "start",
            SEAT,
            "--host",
            "cobalt",
            "--as",
            "person/avery",
        ])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    cobalt.pass();
    cobalt.pass();
    assert_eq!(cobalt.running(), 0, "unreachable source waits by default");
    let result = cobalt
        .cli(&[
            "agents",
            "start",
            SEAT,
            "--source-offline",
            "--as",
            "person/avery",
            "--json",
        ])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let view: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        view["handoff"]["overridden_sources"],
        serde_json::json!(["amber"])
    );
    assert_eq!(view["handoff"]["phase"], "waiting-for-destination");
    let proof = cobalt
        .store
        .claims_for(SEAT, Some(st3::placement::SOURCE_OFFLINE_KIND))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(proof.actor.as_deref(), Some("person/avery"));
    assert_eq!(proof.body["fields"]["destination"], "cobalt");
    assert_eq!(
        proof.body["fields"]["desired_token"],
        view["handoff"]["desired_token"]
    );
    cobalt.pass();
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
    assert_eq!(
        amber.running(),
        1,
        "override is permission, not fabricated stop evidence"
    );
    assert_eq!(
        cobalt.show().await["reachability"],
        "reachable",
        "the explicit exception supersedes the source's older running record for routing"
    );
    let running = cobalt
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .into_iter()
        .find(|c| c.origin == "cobalt" && c.body["fields"]["status"] == "running")
        .unwrap();
    assert!(
        st3::placement::acknowledges(
            &cobalt.store,
            &running,
            &std::collections::BTreeSet::from([proof.id])
        )
        .unwrap()
    );
    let show = cobalt.cli(&["agents", "show", SEAT]).await;
    assert!(String::from_utf8_lossy(&show.stdout).contains("SOURCE OFFLINE amber"));
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    assert_eq!(
        amber.running(),
        0,
        "returning source retires its old runtime"
    );
    amber.sync_to(&cobalt);
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
    assert_eq!(cobalt.show().await["handoff"]["phase"], "running");
}

#[tokio::test]
async fn source_offline_overrides_are_idempotent_and_do_not_release_a_later_move() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    let slate = Node::new("slate").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "first-move");
    let first = cobalt.store.selected_desired_token(SEAT).unwrap().unwrap();
    let proof = override_sources(&cobalt, &first, &["amber"], "override-first")
        .await
        .unwrap();
    assert_eq!(
        override_sources(&cobalt, &first, &["amber"], "override-first")
            .await
            .unwrap()["id"],
        proof["id"]
    );
    assert_eq!(
        cobalt
            .store
            .claims_for(SEAT, Some(st3::placement::SOURCE_OFFLINE_KIND))
            .unwrap()
            .len(),
        1
    );
    cobalt.pass();
    cobalt.pass();
    cobalt.sync_to(&slate);
    slate.declare("slate", true, "second-move");
    slate.pass();
    assert_eq!(slate.running(), 0);
    assert_eq!(
        slate.show().await["handoff"]["overridden_sources"],
        serde_json::json!([])
    );
    slate.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    cobalt.sync_to(&slate);
    slate.pass();
    assert_eq!(
        slate.running(),
        0,
        "the earlier source-offline exception cannot release amber again"
    );
    let second = slate.store.selected_desired_token(SEAT).unwrap().unwrap();
    override_sources(&slate, &second, &["amber"], "override-second")
        .await
        .unwrap();
    slate.pass();
    slate.pass();
    assert_eq!(slate.running(), 1);
}

#[tokio::test]
async fn source_offline_overrides_release_only_the_explicitly_named_sources() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    let slate = Node::new("slate").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "intermediate");
    cobalt.sync_to(&slate);
    slate.declare("slate", true, "destination");
    let token = slate.store.selected_desired_token(SEAT).unwrap().unwrap();
    override_sources(&slate, &token, &["amber"], "amber-offline")
        .await
        .unwrap();
    slate.pass();
    assert_eq!(slate.running(), 0);
    assert_eq!(
        slate.show().await["handoff"]["pending_sources"],
        serde_json::json!(["cobalt"])
    );
    override_sources(&slate, &token, &["cobalt"], "cobalt-offline")
        .await
        .unwrap();
    slate.pass();
    slate.pass();
    assert_eq!(slate.running(), 1);
}

#[tokio::test]
async fn source_offline_override_survives_reversed_replication_and_replay() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    let slate = Node::new("slate").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    let token = cobalt.store.selected_desired_token(SEAT).unwrap().unwrap();
    override_sources(&cobalt, &token, &["amber"], "offline")
        .await
        .unwrap();
    cobalt.pass();
    cobalt.pass();
    let expected = cobalt.show().await["handoff"].clone();
    let exchange = cobalt
        .store
        .export_replication_exchange(FLEET, &slate.store.replication_inventory().unwrap())
        .unwrap();
    for envelope in exchange.envelopes.iter().rev() {
        let mut single = exchange.clone();
        single.envelopes = vec![envelope.clone()];
        slate
            .store
            .receive_replication_exchange("cobalt", FLEET, &single)
            .unwrap();
        Node::admit(&slate.store);
    }
    assert_eq!(slate.show().await["handoff"], expected);
    slate.store.replay_replication_graph().unwrap();
    assert_eq!(slate.show().await["handoff"], expected);
}

#[tokio::test]
async fn source_offline_api_rejects_stale_foreign_and_unrelated_source_requests() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    let old = cobalt.store.selected_desired_token(SEAT).unwrap().unwrap();
    assert!(
        override_sources(&cobalt, &old, &["unknown"], "unknown")
            .await
            .is_err()
    );
    cobalt.declare("cobalt", true, "revision");
    assert!(
        override_sources(&cobalt, &old, &["amber"], "stale")
            .await
            .is_err()
    );
    let token = cobalt.store.selected_desired_token(SEAT).unwrap().unwrap();
    let client = st3::client::Client::unix(cobalt.root.path().join("st3.sock"));
    assert!(client.post::<_, Value>("/v1/agents/source-offline", &serde_json::json!({
        "subject": "agent/example/other", "actor": "person/avery", "desired_token": token,
        "sources": ["amber"], "idempotency_key": "foreign",
    })).await.is_err());
    assert!(
        client
            .post::<_, Value>(
                "/v1/agents/source-offline",
                &serde_json::json!({
                    "subject": SEAT, "actor": "daemon/runtime", "desired_token": token,
                    "sources": ["amber"], "idempotency_key": "wrong-actor",
                })
            )
            .await
            .is_err()
    );
    assert!(client.post::<_, Value>("/v1/claims", &serde_json::json!({
        "subject": SEAT, "kind": st3::placement::SOURCE_OFFLINE_KIND, "actor": "person/avery",
        "fields": {"desired_token": token, "destination": "cobalt", "sources": ["amber"]},
        "evidence": [token],
    })).await.is_err(), "ordinary claim publication cannot bypass the dedicated operation");
    assert!(
        cobalt
            .store
            .claims_for(SEAT, Some(st3::placement::SOURCE_OFFLINE_KIND))
            .unwrap()
            .is_empty()
    );
}
/// Only this fixture's runtimes exist here; no shared seats or daemons are touched.
#[derive(Default)]
struct Runtime {
    observations: Mutex<HashMap<String, RuntimeObservation>>,
    starts: Mutex<Vec<MemberSpec>>,
    stops: Mutex<Vec<String>>,
    refuse_start: Mutex<bool>,
    delay_stop: Mutex<bool>,
    unreadable: Mutex<bool>,
}
impl RuntimeControl for Runtime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<RuntimeObservation>> {
        anyhow::ensure!(
            !*self.unreadable.lock().unwrap(),
            "fixture PTY service unavailable"
        );
        Ok(self
            .observations
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect())
    }
    fn observe_exec(&self, id: &str) -> anyhow::Result<Option<RuntimeObservation>> {
        Ok(self.observations.lock().unwrap().get(id).cloned())
    }
    fn start(&self, member: &MemberSpec) -> anyhow::Result<()> {
        anyhow::ensure!(
            !*self.refuse_start.lock().unwrap(),
            "fixture rejected launch"
        );
        let mut starts = self.starts.lock().unwrap();
        starts.push(member.clone());
        self.observations.lock().unwrap().insert(
            member.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: member.terminal,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(format!("fixture:{}", starts.len())),
            },
        );
        Ok(())
    }
    fn stop(&self, id: &str, _: bool, incarnation: Option<&str>) -> anyhow::Result<()> {
        let mut observations = self.observations.lock().unwrap();
        let current = observations.get_mut(id).unwrap();
        assert_eq!(current.incarnation_id.as_deref(), incarnation);
        self.stops.lock().unwrap().push(incarnation.unwrap().into());
        if !*self.delay_stop.lock().unwrap() {
            current.status = "exited".into();
            current.exit_code = Some(0);
        }
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        panic!("fixture stops immediately")
    }
    fn remove(&self, id: &str, _: bool) -> anyhow::Result<()> {
        self.observations.lock().unwrap().remove(id);
        Ok(())
    }
    fn screen(&self, _: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

struct Node {
    root: tempfile::TempDir,
    store: Arc<Store>,
    runtime: Arc<Runtime>,
    reconciler: Reconciler<Runtime>,
    server: tokio::task::JoinHandle<anyhow::Result<()>>,
    name: String,
}
impl Node {
    async fn new(name: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&root.path().join("graph.db"), name).unwrap());
        store.bind_fleet(FLEET).unwrap();
        let runtime = Arc::new(Runtime::default());
        let notify = Arc::new(Notify::new());
        let reconciler =
            Reconciler::new(store.clone(), runtime.clone(), name.into(), notify.clone());
        let state = AppState {
            store: store.clone(),
            notify,
            event_notify: watch::channel(0).0,
            node: name.into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: Some(FLEET.into()),
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        let socket = root.path().join("st3.sock");
        let router = st3::api::router(state);
        let server = tokio::spawn(async move { st3::api::serve_unix(&socket, router).await });
        for _ in 0..100 {
            if root.path().join("st3.sock").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Self {
            root,
            store,
            runtime,
            reconciler,
            server,
            name: name.into(),
        }
    }
    fn declare(&self, host: &str, terminal: bool, key: &str) {
        let kdl = format!(
            r#"version 2
agent "example/mover" {{
 name "{key}"
 host "{host}"
 workspace {:?}
 command "sleep 1000"
 restart always
}}
"#,
            self.root.path().to_str().unwrap()
        );
        let mut intent = st3::parse_intent(&kdl, &self.name).unwrap();
        // Agent KDL currently selects PTYs; exercise the reconciler's exec backend as well.
        intent
            .subjects
            .get_mut(SEAT)
            .unwrap()
            .member
            .as_mut()
            .unwrap()
            .terminal = terminal;
        let preview = self
            .store
            .mission(
                &intent,
                st3::model::IntentInput {
                    kdl,
                    source_name: None,
                },
            )
            .unwrap();
        self.store
            .apply_as(&intent, &preview.subject_tokens, key, Some("person/avery"))
            .unwrap();
    }
    fn sync_to(&self, other: &Node) {
        for _ in 0..50 {
            let exchange = self
                .store
                .export_replication_exchange(FLEET, &other.store.replication_inventory().unwrap())
                .unwrap();
            if exchange.envelopes.is_empty() {
                return;
            }
            other
                .store
                .receive_replication_exchange(&self.name, FLEET, &exchange)
                .unwrap();
            Self::admit(&other.store);
        }
        panic!("fixture replication did not settle");
    }
    fn admit(store: &Store) {
        store.validate_replication_backlog().unwrap();
        store.apply_replication_repairs().unwrap();
        store.project_replication_backlog().unwrap();
    }
    fn pass(&self) {
        self.reconciler.reconcile_once().unwrap();
    }
    fn running(&self) -> usize {
        self.runtime
            .observations
            .lock()
            .unwrap()
            .values()
            .filter(|o| o.status == "running")
            .count()
    }
    async fn cli(&self, args: &[&str]) -> std::process::Output {
        let socket = self.root.path().join("st3.sock");
        let home = self.root.path().to_owned();
        let args = args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
                .env_remove("ST_AGENT")
                .env_remove("ST_MISSION_RUN")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join("config"))
                .args(["--endpoint", socket.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }
    async fn show(&self) -> Value {
        let client = st3::client::Client::unix(self.root.path().join("st3.sock"));
        client
            .get(&format!("/v1/client/agents/{}", urlencoding::encode(SEAT)))
            .await
            .unwrap()
    }
}
impl Drop for Node {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn move_running_seat(terminal: bool) {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", terminal, "initial");
    amber.pass();
    amber.pass();
    assert_eq!(amber.running(), 1);
    let source_running = amber
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .into_iter()
        .find(|c| c.body["fields"]["status"] == "running")
        .unwrap();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", terminal, "move");
    cobalt.sync_to(&amber);
    assert_eq!(
        amber.store.selected_desired_token(SEAT).unwrap(),
        cobalt.store.selected_desired_token(SEAT).unwrap()
    );
    assert_eq!(
        amber
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|d| d.subject == SEAT)
            .unwrap()
            .member
            .unwrap()
            .host,
        "cobalt"
    );
    // Evaluate the destination first: replication of placement is not proof of a stopped source.
    cobalt.pass();
    cobalt.pass();
    assert_eq!(
        cobalt.running(),
        0,
        "destination launched while source was still running"
    );
    let waiting = cobalt.show().await;
    assert_eq!(waiting["handoff"]["phase"], "stopping-source");
    assert_eq!(waiting["state"], "waiting");
    assert!(waiting["incarnation_id"].is_null());
    amber.pass();
    amber.pass();
    assert_eq!(
        amber.running(),
        0,
        "placement-away left the old runtime alive"
    );
    // A local stop is insufficient until its observation reaches the destination.
    cobalt.pass();
    assert_eq!(cobalt.running(), 0);
    amber.sync_to(&cobalt);
    assert_eq!(
        cobalt.show().await["handoff"]["phase"],
        "waiting-for-destination"
    );
    cobalt.pass();
    assert_eq!(cobalt.show().await["handoff"]["phase"], "starting");
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
    assert_eq!(cobalt.show().await["handoff"]["phase"], "running");
    assert_eq!(amber.runtime.stops.lock().unwrap().len(), 1);
    let stopped = amber
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .into_iter()
        .find(|c| {
            c.body.pointer("/fields/reason").and_then(Value::as_str) == Some("placed-elsewhere")
        })
        .unwrap();
    assert_eq!(stopped.body["fields"]["status"], "stopped");
    assert_eq!(stopped.body["fields"]["incarnation_id"], "fixture:1");
    assert!(
        st3::placement::acknowledges(
            &amber.store,
            &stopped,
            &std::collections::BTreeSet::from([source_running.id.clone()]),
        )
        .unwrap(),
        "source stop descends from its running observation: {stopped:?}, {source_running:?}"
    );
    let destination_running = cobalt
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .into_iter()
        .find(|c| c.origin == "cobalt" && c.body["fields"]["status"] == "running")
        .unwrap();
    assert!(
        st3::placement::acknowledges(
            &cobalt.store,
            &destination_running,
            &std::collections::BTreeSet::from([stopped.id]),
        )
        .unwrap(),
        "destination start descends from the source stop proof"
    );
}
#[tokio::test]
async fn running_pty_moves_only_after_replicated_source_stop() {
    move_running_seat(true).await;
}
#[tokio::test]
async fn running_exec_moves_only_after_replicated_source_stop() {
    move_running_seat(false).await;
}

#[tokio::test]
async fn a_termination_request_does_not_release_the_destination() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.sync_to(&amber);
    *amber.runtime.delay_stop.lock().unwrap() = true;
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running(), 1);
    assert_eq!(cobalt.running(), 0, "SIGTERM is not proof of process exit");
    for o in amber.runtime.observations.lock().unwrap().values_mut() {
        o.status = "exited".into();
        o.exit_code = Some(0);
    }
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn unreadable_source_snapshot_never_counts_as_a_stop() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.sync_to(&amber);
    *amber.runtime.unreadable.lock().unwrap() = true;
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    assert_eq!(amber.running(), 1);
    assert_eq!(cobalt.running(), 0);
    *amber.runtime.unreadable.lock().unwrap() = false;
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running(), 0);
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn source_stops_even_when_the_selected_actual_belongs_to_the_destination() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.sync_to(&amber);
    // Model an already duplicated seat from an older destination build.
    let member = cobalt
        .store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|d| d.subject == SEAT)
        .unwrap()
        .member
        .unwrap();
    cobalt.runtime.start(&member).unwrap();
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("status".into(), Value::String("running".into()));
    fields.insert("runtime_id".into(), Value::String(member.runtime_id));
    fields.insert("host".into(), Value::String("cobalt".into()));
    fields.insert("incarnation_id".into(), Value::String("fixture:1".into()));
    cobalt
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    cobalt.sync_to(&amber);
    assert_eq!(
        amber.store.selected_actual_origin(SEAT).unwrap().as_deref(),
        Some("cobalt")
    );
    amber.pass();
    amber.pass();
    assert_eq!(amber.running(), 0);
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn an_unlaunched_intermediate_host_must_acknowledge_before_a_third_host_starts() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    let jade = Node::new("jade").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "first-move");
    cobalt.sync_to(&jade);
    jade.declare("jade", true, "second-move");
    jade.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&jade);
    jade.pass();
    assert_eq!(
        jade.running(),
        0,
        "intermediate source might still launch its stale placement"
    );
    jade.sync_to(&cobalt);
    cobalt.pass();
    cobalt.sync_to(&jade);
    jade.pass();
    jade.pass();
    assert_eq!(jade.running(), 1);
    assert_eq!(amber.running() + cobalt.running(), 0);
}

#[tokio::test]
async fn moving_back_requires_a_new_acknowledgement_from_the_last_destination() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "outbound");
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    cobalt.sync_to(&amber);
    amber.declare("amber", true, "return");
    amber.sync_to(&cobalt);
    amber.pass();
    assert_eq!(amber.running(), 0);
    cobalt.pass();
    cobalt.pass();
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    assert_eq!(amber.running(), 1);
    assert_eq!(cobalt.running(), 0);
    // Same-host revisions retain the acknowledgement rather than creating another stop.
    let before = cobalt
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .into_iter()
        .filter(|c| c.origin == "cobalt")
        .count();
    amber.declare("amber", true, "same-host");
    amber.sync_to(&cobalt);
    cobalt.pass();
    assert_eq!(
        cobalt
            .store
            .claims_for(SEAT, Some("runtime.observed"))
            .unwrap()
            .into_iter()
            .filter(|c| c.origin == "cobalt")
            .count(),
        before
    );
}

#[tokio::test]
async fn handoff_projection_survives_replay_and_reversed_replication() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    let slate = Node::new("slate").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    let expected = cobalt.show().await["handoff"].clone();
    let exchange = cobalt
        .store
        .export_replication_exchange(FLEET, &slate.store.replication_inventory().unwrap())
        .unwrap();
    for envelope in exchange.envelopes.iter().rev() {
        let mut single = exchange.clone();
        single.envelopes = vec![envelope.clone()];
        slate
            .store
            .receive_replication_exchange("cobalt", FLEET, &single)
            .unwrap();
        Node::admit(&slate.store);
    }
    assert_eq!(slate.show().await["handoff"], expected);
    slate.store.replay_replication_graph().unwrap();
    assert_eq!(slate.show().await["handoff"], expected);
}

#[tokio::test]
async fn a_confirmed_explicit_stop_allows_a_move_while_the_source_is_offline() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    let intent = st3::parse_intent(&format!("version 2\nstop \"{SEAT}\"\n"), "amber").unwrap();
    let preview = amber
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: format!("version 2\nstop \"{SEAT}\"\n"),
                source_name: None,
            },
        )
        .unwrap();
    amber
        .store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "stop",
            Some("person/avery"),
        )
        .unwrap();
    amber.pass();
    amber.pass();
    assert_eq!(amber.running(), 0);
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    // The source takes no further passes or replication exchanges.
    cobalt.pass();
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn agents_start_and_show_report_the_pending_handoff() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    let result = amber
        .cli(&[
            "agents",
            "start",
            SEAT,
            "--host",
            "cobalt",
            "--as",
            "person/avery",
        ])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains("stopping-source"), "{text}");
    let shown = amber.cli(&["agents", "show", SEAT]).await;
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let text = String::from_utf8(shown.stdout).unwrap();
    assert!(
        text.contains("HANDOFF") && text.contains("stopping-source"),
        "{text}"
    );
    assert!(!text.contains("INCARNATION"), "{text}");
    amber.sync_to(&cobalt);
    cobalt.pass();
    assert_eq!(cobalt.running(), 0);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running(), 0);
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn agents_apply_moves_are_fenced_and_start_json_names_the_phase() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    let path = cobalt.root.path().join("move.kdl");
    std::fs::write(
        &path,
        format!(
            r#"version 2
agent "example/mover" {{
 host "cobalt"
 workspace {:?}
 command "sleep 1000"
 restart always
}}
"#,
            cobalt.root.path().to_str().unwrap()
        ),
    )
    .unwrap();
    let result = cobalt
        .cli(&[
            "agents",
            "apply",
            path.to_str().unwrap(),
            "--as",
            "person/avery",
        ])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    cobalt.pass();
    assert_eq!(cobalt.running(), 0);
    let result = cobalt
        .cli(&["--json", "agents", "start", SEAT, "--as", "person/avery"])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["handoff"]["phase"], "stopping-source");
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running(), 0);
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn stopping_a_move_before_handoff_finishes_retires_both_local_copies() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    // An old destination build started early; stop must still retire each local incarnation.
    let member = cobalt
        .store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .find(|d| d.subject == SEAT)
        .unwrap()
        .member
        .unwrap();
    cobalt.runtime.start(&member).unwrap();
    let fields = std::collections::BTreeMap::from([
        ("status".into(), Value::String("running".into())),
        (
            "runtime_id".into(),
            Value::String(member.runtime_id.clone()),
        ),
        ("host".into(), Value::String("cobalt".into())),
        ("terminal".into(), Value::Bool(true)),
        ("incarnation_id".into(), Value::String("fixture:1".into())),
    ]);
    cobalt
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let result = cobalt
        .cli(&["agents", "stop", SEAT, "--as", "person/avery"])
        .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running() + cobalt.running(), 0);
    // A settled stop must not append more observations on a later pass.
    let amber_before = amber
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .len();
    let cobalt_before = cobalt
        .store
        .claims_for(SEAT, Some("runtime.observed"))
        .unwrap()
        .len();
    amber.pass();
    cobalt.pass();
    assert_eq!(
        amber
            .store
            .claims_for(SEAT, Some("runtime.observed"))
            .unwrap()
            .len(),
        amber_before
    );
    assert_eq!(
        cobalt
            .store
            .claims_for(SEAT, Some("runtime.observed"))
            .unwrap()
            .len(),
        cobalt_before
    );
}

#[tokio::test]
async fn legacy_stopped_observations_keep_their_causal_stop_proof() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    let kdl = format!("version 2\nstop \"{SEAT}\"\n");
    let intent = st3::parse_intent(&kdl, "amber").unwrap();
    let preview = amber
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl,
                source_name: None,
            },
        )
        .unwrap();
    amber
        .store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "stop",
            Some("person/avery"),
        )
        .unwrap();
    amber.pass();
    amber.pass();
    // A legacy runtime observation carries no declaration field or explicit evidence;
    // its causal predecessors still retain the source's confirmed stop.
    amber
        .store
        .append_claim(&st3::model::ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: None,
            fields: std::collections::BTreeMap::from([(
                "status".into(),
                Value::String("stopped".into()),
            )]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.pass();
    cobalt.pass();
    assert_eq!(cobalt.running(), 1);
}

#[tokio::test]
async fn a_reappearing_source_incarnation_revokes_an_older_stop_acknowledgement() {
    let amber = Node::new("amber").await;
    let cobalt = Node::new("cobalt").await;
    amber.declare("amber", true, "initial");
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.declare("cobalt", true, "move");
    cobalt.sync_to(&amber);
    amber.pass();
    amber.pass();
    amber.sync_to(&cobalt);
    // Before destination launch, a returning registry reveals another local incarnation.
    for observation in amber.runtime.observations.lock().unwrap().values_mut() {
        observation.status = "running".into();
        observation.incarnation_id = Some("returning-incarnation".into());
    }
    *amber.runtime.delay_stop.lock().unwrap() = true;
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    assert_eq!(amber.running(), 1);
    assert_eq!(cobalt.running(), 0);
    for observation in amber.runtime.observations.lock().unwrap().values_mut() {
        observation.status = "exited".into();
    }
    amber.pass();
    amber.sync_to(&cobalt);
    cobalt.pass();
    cobalt.pass();
    assert_eq!(amber.running(), 0);
    assert_eq!(cobalt.running(), 1);
}
