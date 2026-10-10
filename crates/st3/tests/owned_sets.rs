//! Each fixture serves an isolated daemon over its own Unix socket and durable graph.
use std::{path::Path, sync::Arc, time::Duration};

use serde_json::{Value, json};
use st3::{
    api::AppState,
    client::Client,
    fleet::MemberKey,
    model::{ClaimInput, IntentInput},
    store::{
        Store,
        owned_sets::{Options, Preview, Request, Source},
    },
};
use tokio::sync::{Notify, watch};

const FLEET: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

struct Daemon {
    store: Arc<Store>,
    client: Client,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn daemon(root: &Path, name: &str, key: Arc<MemberKey>, anchor: &MemberKey) -> Daemon {
    let root = root.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root.join("graph.sqlite3"), name).unwrap());
    store.bind_fleet(FLEET).unwrap();
    store.pin_fleet_anchor(anchor.public()).unwrap();
    store.set_member_key(Some(key)).unwrap();
    append(
        &store,
        &format!("daemon/{name}"),
        "daemon.started",
        json!({"status":"running","features":{"owned_sets":1}}),
    );
    let socket = root.join("st3.sock");
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: name.into(),
        state_dir: root.clone(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: Some(FLEET.into()),
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, st3::api::router(state))
            .await
            .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    Daemon {
        store,
        client: Client::unix_as(socket, "person/operator").unwrap(),
        server,
    }
}

fn append(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn share(from: &Store, to: &Store, peer: &str) {
    let exchange = from
        .export_replication_exchange_answering(
            FLEET,
            &to.replication_inventory().unwrap(),
            &to.replication_signature_requests().unwrap(),
        )
        .unwrap();
    to.receive_replication_exchange(peer, FLEET, &exchange)
        .unwrap();
    let admission = to.validate_replication_backlog().unwrap();
    assert_eq!((admission.invalid, admission.unknown), (0, 0));
    to.project_replication_backlog().unwrap();
}

fn bundle(note: &str, second: bool) -> String {
    let mut kdl = format!(
        "version 2\nagent \"garden/orchard\" {{\n workspace \".\"\n command \"true\"\n description \"{note}\"\n}}\n"
    );
    if second {
        kdl.push_str("agent \"garden/meadow\" {\n workspace \".\"\n command \"true\"\n}\n");
    }
    kdl
}

async fn request(daemon: &Daemon, sequence: u64, kdl: String) -> Request {
    let selected = daemon.store.owned_sets().unwrap().into_iter().next();
    Request {
        intent: IntentInput {
            kdl,
            source_name: None,
        },
        options: Options {
            rollout: None,
            set: "garden".into(),
            source: Source {
                repository: "acme/garden".into(),
                r#ref: "refs/heads/main".into(),
                sha: format!("{sequence:040x}"),
                sequence,
            },
            expected_set: selected.map_or("absent".into(), |v| v.revision),
            adopt: Default::default(),
            allow_empty: false,
            confirm_retire: None,
            expected_subjects: Default::default(),
        },
        actor: "person/operator".into(),
        idempotency_key: format!("publish-{sequence}"),
    }
}

async fn preview(daemon: &Daemon, request: &mut Request) -> Preview {
    let preview: Preview = daemon
        .client
        .post("/v1/sets/preview", request)
        .await
        .unwrap();
    assert!(preview.blockers.is_empty(), "{preview:?}");
    request.options.expected_subjects = preview.expected_subjects.clone();
    preview
}

async fn publish(daemon: &Daemon, sequence: u64, note: &str) {
    let mut request = request(daemon, sequence, bundle(note, true)).await;
    preview(daemon, &mut request).await;
    let result: Value = daemon
        .client
        .post("/v1/sets/apply", &request)
        .await
        .unwrap();
    assert_eq!(result["publication"]["changed"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_daemons_heal_to_newest_source_and_pruning_requires_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let keys = (0..3)
        .map(|_| Arc::new(MemberKey::generate().unwrap().0))
        .collect::<Vec<_>>();
    let amber = daemon(root.path(), "amber", keys[0].clone(), &keys[0]).await;
    let cobalt = daemon(root.path(), "cobalt", keys[1].clone(), &keys[0]).await;
    let ivory = daemon(root.path(), "ivory", keys[2].clone(), &keys[0]).await;
    for (i, name) in ["amber", "cobalt", "ivory"].into_iter().enumerate() {
        let mut fields = json!({"fleet_id":FLEET,"member_key":keys[i].public(),
            "via":if i==0{"anchor"}else{"invite"},"mode":"listening"});
        if i != 0 {
            fields["sponsor"] = json!("host/amber");
        }
        append(
            &amber.store,
            &format!("host/{name}"),
            "fleet.member-admitted",
            fields,
        );
    }
    for (from, name) in [(&amber, "amber"), (&cobalt, "cobalt"), (&ivory, "ivory")] {
        for to in [&amber, &cobalt, &ivory] {
            share(&from.store, &to.store, name);
        }
    }
    publish(&amber, 10, "initial").await;
    share(&amber.store, &cobalt.store, "amber");
    share(&amber.store, &ivory.store, "amber");
    publish(&amber, 30, "newer").await;
    publish(&cobalt, 20, "older").await;
    share(&amber.store, &ivory.store, "amber");
    share(&cobalt.store, &ivory.store, "cobalt"); // Late older render.
    share(&cobalt.store, &amber.store, "cobalt");
    share(&amber.store, &cobalt.store, "amber");
    let token = amber
        .store
        .selected_desired_token("agent/garden/orchard")
        .unwrap()
        .unwrap();
    for d in [&amber, &cobalt, &ivory] {
        let response: Value = d.client.get("/v1/client/sets/garden").await.unwrap();
        assert_eq!(response["receipt"]["source"]["sequence"], 30);
        assert_eq!(
            d.store
                .selected_desired_token("agent/garden/orchard")
                .unwrap()
                .as_deref(),
            Some(token.as_str())
        );
        assert!(
            d.store
                .status(Some("agent/garden/orchard"))
                .unwrap()
                .subjects[0]
                .conflicts
                .is_empty()
        );
        // Typed generated clients understand the additive owned-set resource.
        serde_json::from_value::<st3_client::Resource>(response).unwrap();
    }
    let selected = amber.store.owned_sets().unwrap().remove(0);
    for (subject, member) in &selected.receipt.members {
        let incarnation = format!("launch-{subject}");
        append(
            &amber.store,
            subject,
            "runtime.action.succeeded",
            json!({
                "action":"start", "incarnation_id":incarnation, "desired_token":member.claim,
            }),
        );
        append(
            &amber.store,
            subject,
            "runtime.observed",
            json!({
                "status":"running", "incarnation_id":incarnation, "runtime_id":subject,
            }),
        );
    }
    let running: Value = amber
        .client
        .get(&format!("/v1/client/sets/garden?sha={:040x}", 30))
        .await
        .unwrap();
    assert_eq!(running["commit_status"]["running"], true);
    for member in running["members_status"].as_array().unwrap() {
        assert_eq!(member["desired_token"], member["launched_token"]);
        assert_eq!(member["rollout"], "running");
    }
    let mut stale = request(&ivory, 25, bundle("late", true)).await;
    let stale_preview: Preview = ivory.client.post("/v1/sets/preview", &stale).await.unwrap();
    assert!(!stale_preview.blockers.is_empty());
    stale.options.expected_subjects = stale_preview.expected_subjects;
    assert!(
        ivory
            .client
            .post::<_, Value>("/v1/sets/apply", &stale)
            .await
            .is_err()
    );

    let mut prune = request(&amber, 40, bundle("newer", false)).await;
    let p = preview(&amber, &mut prune).await;
    assert!(p.mass_retirement);
    let before = amber.store.owned_sets().unwrap()[0].revision.clone();
    let refusal = amber
        .client
        .post::<_, Value>("/v1/sets/apply", &prune)
        .await
        .unwrap_err();
    assert!(
        refusal.to_string().contains("mass-retirement-refused"),
        "{refusal}"
    );
    assert_eq!(amber.store.owned_sets().unwrap()[0].revision, before);
    prune.options.confirm_retire = Some(p.digest);
    amber
        .client
        .post::<_, Value>("/v1/sets/apply", &prune)
        .await
        .unwrap();
    share(&amber.store, &cobalt.store, "amber");
    share(&amber.store, &ivory.store, "amber");
    for d in [&amber, &cobalt, &ivory] {
        let response: Value = d.client.get("/v1/client/sets/garden").await.unwrap();
        assert_eq!(response["receipt"]["source"]["sequence"], 40);
        assert!(response["receipt"]["retired"]["agent/garden/meadow"].is_object());
        assert_eq!(
            d.store
                .desired_subject_with_writer("agent/garden/meadow")
                .unwrap()
                .unwrap()
                .0
                .kind,
            "stop"
        );
        let old: Value = d
            .client
            .get(&format!("/v1/client/sets/garden?sha={:040x}", 30))
            .await
            .unwrap();
        assert_eq!(old["commit_status"]["superseded"], true);
        assert_eq!(old["commit_status"]["running"], false);
    }

    // The executable reads every specified file before publication and supports multi-file bundles.
    let previous = amber.store.owned_sets().unwrap()[0].revision.clone();
    let orchard = root.path().join("orchard.kdl");
    let blossom = root.path().join("blossom.kdl");
    std::fs::write(&orchard, bundle("newer", false)).unwrap();
    std::fs::write(
        &blossom,
        "version 2\nagent \"garden/blossom\" { command \"true\" }\n",
    )
    .unwrap();
    let run = |files: Vec<std::path::PathBuf>, dry: bool| {
        let socket = amber.client.socket_path().unwrap().to_path_buf();
        let previous = previous.clone();
        tokio::task::spawn_blocking(move || {
            let mut command =
                st3::test_support::command(test_bin!("st3-fixture"));
            command
                .env_remove("ST_AGENT")
                .env_remove("ST_MISSION_RUN")
                .args(["--json", "--endpoint"])
                .arg(socket)
                .args(["apply", "--set", "garden"])
                .args(files)
                .args([
                    "--repository",
                    "acme/garden",
                    "--ref",
                    "refs/heads/main",
                    "--sha",
                ])
                .arg(format!("{:040x}", 50))
                .args(["--source-sequence", "50", "--expect-set"])
                .arg(previous)
                .args(["--as", "person/operator"]);
            if dry {
                command.arg("--dry-run");
            }
            command.output().unwrap()
        })
    };
    let missing = run(
        vec![orchard.clone(), root.path().join("missing.kdl")],
        false,
    )
    .await
    .unwrap();
    assert!(!missing.status.success());
    assert_eq!(amber.store.owned_sets().unwrap()[0].revision, previous);
    let dry = run(vec![orchard.clone(), blossom.clone()], true)
        .await
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry: Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(dry["changes"]["agent/garden/blossom"], "added");
    let applied = run(vec![orchard, blossom], false).await.unwrap();
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        amber.store.owned_sets().unwrap()[0].receipt.source.sequence,
        50
    );
    let mut mixed = bundle("mixed", false);
    mixed.push_str("agent \"garden/blossom\" { command \"true\" }\n\
        mission \"harvest\" state=\"ready\" { goal \"Harvest\"; step \"work\" { assigned-to \"agent/garden/orchard\" } }\n\
        schedule \"garden/daily\" { every \"6h\"; anchor \"2026-01-01T00:00:00Z\"; work { mission \"harvest\"; workspace \"/tmp\"; } }\n");
    let mut mixed = request(&amber, 60, mixed).await;
    preview(&amber, &mut mixed).await;
    let correct_heads = mixed.options.expected_subjects.clone();
    mixed
        .options
        .expected_subjects
        .insert("mission/harvest".into(), vec!["stale".into()]);
    let before = amber.store.owned_sets().unwrap()[0].revision.clone();
    assert!(
        amber
            .client
            .post::<_, Value>("/v1/sets/apply", &mixed)
            .await
            .is_err()
    );
    assert_eq!(amber.store.owned_sets().unwrap()[0].revision, before);
    assert!(amber.store.mission_spec("harvest", None).unwrap().is_none());
    assert!(
        amber
            .store
            .selected_desired_token("schedule/garden/daily")
            .unwrap()
            .is_none()
    );
    mixed.options.expected_subjects = correct_heads;
    amber
        .client
        .post::<_, Value>("/v1/sets/apply", &mixed)
        .await
        .unwrap();
    share(&amber.store, &ivory.store, "amber");
    let selected = ivory.store.owned_sets().unwrap().remove(0);
    assert!(selected.blockers.is_empty());
    assert_eq!(selected.receipt.members["mission/harvest"].kind, "mission");
    assert_eq!(
        selected.receipt.members["schedule/garden/daily"].kind,
        "schedule"
    );
    let mut omit = request(&amber, 70, bundle("mixed", false)).await;
    let p = preview(&amber, &mut omit).await;
    omit.options.confirm_retire = Some(p.digest);
    amber
        .client
        .post::<_, Value>("/v1/sets/apply", &omit)
        .await
        .unwrap();
    share(&amber.store, &ivory.store, "amber");
    let selected = ivory.store.owned_sets().unwrap().remove(0);
    assert!(selected.blockers.is_empty(), "{:?}", selected.blockers);
    let schedule = ivory
        .store
        .desired_subject_with_writer("schedule/garden/daily")
        .unwrap()
        .unwrap()
        .0;
    assert!(
        st3::graph::schedule_spec(&schedule.desired, "ivory")
            .unwrap()
            .stopped
    );
    assert_eq!(
        ivory
            .store
            .mission_spec("harvest", None)
            .unwrap()
            .unwrap()
            .state,
        st3::model::MissionState::Retired
    );
    let launched_token = amber
        .store
        .selected_desired_token("agent/garden/orchard")
        .unwrap()
        .unwrap();
    append(
        &amber.store,
        "agent/garden/orchard",
        "runtime.action.succeeded",
        json!({
            "action":"start", "incarnation_id":"launch-agent/garden/orchard",
            "desired_token":launched_token,
        }),
    );
    let labeled = bundle("mixed", false).replace(
        "description \"mixed\"",
        "description \"mixed\"\n name \"Orchard\"",
    );
    let mut labeled = request(&amber, 80, labeled).await;
    preview(&amber, &mut labeled).await;
    amber
        .client
        .post::<_, Value>("/v1/sets/apply", &labeled)
        .await
        .unwrap();
    let status: Value = amber.client.get("/v1/client/sets/garden").await.unwrap();
    let orchard = status["members_status"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"] == "agent/garden/orchard")
        .unwrap();
    assert_eq!(orchard["rollout"], "running");
    assert_eq!(orchard["launch_current"], true);
    assert_eq!(orchard["launched_token"], launched_token);
    assert_ne!(orchard["desired_token"], orchard["launched_token"]);
    for args in [
        vec!["ls".to_owned()],
        vec!["show".to_owned(), "garden".to_owned()],
        vec![
            "status".to_owned(),
            "garden".to_owned(),
            "--sha".to_owned(),
            format!("{:040x}", 80),
        ],
    ] {
        let socket = root.path().join("amber/st3.sock");
        let output = tokio::task::spawn_blocking(move || {
            st3::test_support::command(test_bin!("st3-fixture"))
                .env_remove("ST_AGENT")
                .env_remove("ST_MISSION_RUN")
                .args(["--json", "--endpoint"])
                .arg(socket)
                .arg("sets")
                .args(args)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let selected = value["value"]["items"]
            .as_array()
            .map_or(&value["value"], |items| &items[0]);
        assert_eq!(selected["receipt"]["source"]["sequence"], 80);
    }
}

/// PTY process identities and exits are controlled independently of the isolated HTTP daemon.
#[derive(Default)]
struct RolloutRuntime {
    observations:
        std::sync::Mutex<std::collections::BTreeMap<String, st3::reconcile::RuntimeObservation>>,
    starts: std::sync::Mutex<Vec<st3::model::MemberSpec>>,
    stops: std::sync::Mutex<Vec<String>>,
}
impl st3::reconcile::RuntimeControl for RolloutRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
        Ok(self
            .observations
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect())
    }
    fn observe_exec(&self, id: &str) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
        Ok(self.observations.lock().unwrap().get(id).cloned())
    }
    fn start(&self, member: &st3::model::MemberSpec) -> anyhow::Result<()> {
        let mut starts = self.starts.lock().unwrap();
        starts.push(member.clone());
        self.observations.lock().unwrap().insert(
            member.runtime_id.clone(),
            st3::reconcile::RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: member.terminal,
                status: "running".into(),
                incarnation_id: Some(format!("replacement-{}", starts.len())),
                exit_code: None,
            },
        );
        Ok(())
    }
    fn stop(&self, id: &str, _: bool, expected: Option<&str>) -> anyhow::Result<()> {
        let mut observations = self.observations.lock().unwrap();
        let observed = observations.get_mut(id).unwrap();
        anyhow::ensure!(
            observed.incarnation_id.as_deref() == expected,
            "stale incarnation"
        );
        self.stops.lock().unwrap().push(expected.unwrap().into());
        observed.status = "exited".into();
        Ok(())
    }
    fn kill(&self, id: &str, terminal: bool, expected: Option<&str>) -> anyhow::Result<()> {
        self.stop(id, terminal, expected)
    }
    fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
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

fn native_observation(store: &Store, subject: &str, incarnation: &str) {
    append(
        store,
        subject,
        "harness.session-file",
        json!({"harness":"claude", "session_id":format!("native-{}", subject.replace('/', "-")), "incarnation_id":incarnation}),
    );
    append(
        store,
        subject,
        "harness.observed",
        json!({"state":"idle", "driver":"claude", "incarnation_id":incarnation, "quiescent":true}),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_rollout_publishes_with_automatic_member_and_only_moves_on_explicit_cli_request() {
    use st3::reconcile::{Reconciler, RuntimeControl, RuntimeObservation};
    let root = tempfile::tempdir().unwrap();
    let anchor = Arc::new(MemberKey::generate().unwrap().0);
    let d = daemon(root.path(), "amber", anchor.clone(), &anchor).await;
    append(
        &d.store,
        "daemon/amber",
        "daemon.started",
        json!({"status":"running", "features":{"owned_sets":1,"seat_rollout":1,"seat_rollout_manual":1}}),
    );
    let runtime = Arc::new(RolloutRuntime::default());
    let reconciler = Reconciler::new(
        d.store.clone(),
        runtime.clone(),
        "amber".into(),
        Arc::new(Notify::new()),
    );
    let manual = "agent/garden/orchard";
    let automatic = "agent/garden/meadow";
    let bundle = |model: &str, manual_policy: bool| {
        let mut source = "version 2\n".to_owned();
        for (name, held) in [("orchard", manual_policy), ("meadow", false)] {
            let workspace = root.path().join(name);
            std::fs::create_dir_all(&workspace).unwrap();
            source.push_str(&format!("agent \"garden/{name}\" {{ host \"amber\"; workspace {:?}; {} harness \"claude\" {{ model {model:?}; }}; render {{ file \"active-model\" {model:?}; }} }}\n",
                workspace.display().to_string(), if held {"rollout \"manual\";"} else {""}));
        }
        source
    };
    let mut initial = request(&d, 1, bundle("first", true)).await;
    initial.options.rollout = Some(st3::rollout::Policy::when_idle(1_800_000, false));
    preview(&d, &mut initial).await;
    d.client
        .post::<_, Value>("/v1/sets/apply", &initial)
        .await
        .unwrap();
    // Seed the two running incarnations as independent runtime evidence, not status synthesis.
    for subject in [manual, automatic] {
        let desired = d
            .store
            .desired_subject_with_writer(subject)
            .unwrap()
            .unwrap()
            .0;
        let member = desired.member.unwrap();
        let incarnation = format!("original-{subject}");
        runtime.observations.lock().unwrap().insert(
            member.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                incarnation_id: Some(incarnation.clone()),
                exit_code: None,
            },
        );
        append(
            &d.store,
            subject,
            "runtime.action.succeeded",
            json!({"action":"start", "desired_token":d.store.selected_desired_token(subject).unwrap().unwrap(), "incarnation_id":incarnation}),
        );
        append(
            &d.store,
            subject,
            "runtime.observed",
            json!({"status":"running", "host":"amber", "runtime_id":member.runtime_id, "terminal":true,"incarnation_id":incarnation}),
        );
        native_observation(&d.store, subject, &incarnation);
    }
    reconciler.reconcile_once().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("orchard/active-model")).unwrap(),
        "first"
    );
    let unpublished = d
        .client
        .get::<Value>(&format!("/v1/client/sets/garden?sha={:040x}", 2))
        .await
        .unwrap_err();
    assert!(
        unpublished.to_string().contains("no receipt"),
        "{unpublished}"
    );
    assert!(
        d.client
            .get::<Value>("/v1/client/sets/garden")
            .await
            .is_ok()
    );
    let mut change = request(&d, 2, bundle("second", true)).await;
    change.options.rollout = initial.options.rollout.clone();
    let p = preview(&d, &mut change).await;
    assert!(p.effects[manual].contains("pending (manual)"));
    let change_digest = p.digest.clone();
    d.client
        .post::<_, Value>("/v1/sets/apply", &change)
        .await
        .unwrap();
    for _ in 0..8 {
        reconciler.reconcile_once().unwrap();
        if let Some(operation) = d.store.rollout(automatic).unwrap()
            && operation.drain_ack.is_none()
        {
            st3::rollout::phase(&d.store, automatic, &operation, "drain-ack", None, &[]).unwrap();
        }
        for observation in runtime.snapshot_ptys().unwrap() {
            if observation.status == "running"
                && observation
                    .incarnation_id
                    .as_deref()
                    .unwrap()
                    .starts_with("replacement-")
            {
                native_observation(
                    &d.store,
                    automatic,
                    observation.incarnation_id.as_deref().unwrap(),
                );
            }
        }
    }
    assert_eq!(
        d.store.rollout(automatic).unwrap().unwrap().phase,
        "running"
    );
    assert!(d.store.rollout(manual).unwrap().is_none());
    assert_eq!(runtime.starts.lock().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.path().join("orchard/active-model")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("meadow/active-model")).unwrap(),
        "second"
    );
    assert_eq!(
        *runtime.stops.lock().unwrap(),
        vec![format!("original-{automatic}")]
    );
    assert!(
        st3::rollout::hold_render(
            &d.store,
            &d.store
                .desired_subject_with_writer(manual)
                .unwrap()
                .unwrap()
                .0
        )
        .unwrap()
    );
    let status: Value = d
        .client
        .get(&format!("/v1/client/sets/garden?sha={:040x}", 2))
        .await
        .unwrap();
    assert_eq!(status["commit_status"]["published"], true);
    assert_eq!(status["commit_status"]["satisfied"], true);
    assert_eq!(status["commit_status"]["running"], false);
    let member = status["members_status"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"] == manual)
        .unwrap();
    assert_eq!(member["rollout"], "pending");
    assert_eq!(member["rollout_mode"], "manual");
    assert_eq!(
        member["publication_status"],
        "published, rollout pending (manual)"
    );
    assert_eq!(member["incarnation"], format!("original-{manual}"));
    serde_json::from_value::<st3_client::Resource>(status).unwrap();
    let agent: Value = d
        .client
        .get(&format!("/v1/client/agents/{manual}"))
        .await
        .unwrap();
    assert_eq!(agent["state"], "running");
    assert_eq!(agent["rollout"]["mode"], "manual");
    assert_eq!(
        agent["rollout"]["status"],
        "published, rollout pending (manual)"
    );
    serde_json::from_value::<st3_client::Resource>(agent).unwrap();
    // Neither restarting nor suspension may sneak around the pending publication.
    for path in ["/v1/agents/restart", "/v1/agents/suspend"] {
        let error = d
            .client
            .post::<_, Value>(
                path,
                &json!({"subject":manual,"actor":"person/operator","idempotency_key":path}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("rollout-in-progress"), "{error}");
    }
    let socket = d.client.socket_path().unwrap().to_path_buf();
    let (shown, output) = tokio::task::spawn_blocking(move || {
        let command = || {
            let mut command =
                st3::test_support::command(test_bin!("st3-fixture"));
            command
                .env_remove("ST_AGENT")
                .env_remove("ST_MISSION_RUN")
                .args(["--endpoint"])
                .arg(&socket);
            command
        };
        let shown = command().args(["agents", "show", manual]).output().unwrap();
        let rolled = command()
            .args([
                "--json",
                "agents",
                "rollout",
                manual,
                "--as",
                "person/operator",
            ])
            .output()
            .unwrap();
        (shown, rolled)
    })
    .await
    .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(String::from_utf8_lossy(&shown.stdout).contains("published, rollout pending (manual)"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for _ in 0..8 {
        reconciler.reconcile_once().unwrap();
        let operation = d.store.rollout(manual).unwrap().unwrap();
        if operation.drain_ack.is_none() {
            st3::rollout::phase(&d.store, manual, &operation, "drain-ack", None, &[]).unwrap();
        }
        if let Some(incarnation) = operation.replacement_incarnation {
            native_observation(&d.store, manual, &incarnation);
        }
    }
    assert_eq!(d.store.rollout(manual).unwrap().unwrap().phase, "running");
    assert_eq!(runtime.starts.lock().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("orchard/active-model")).unwrap(),
        "second"
    );
    let final_status: Value = d
        .client
        .get(&format!("/v1/client/sets/garden?sha={:040x}", 2))
        .await
        .unwrap();
    assert_eq!(final_status["commit_status"]["running"], true);
    assert_eq!(final_status["commit_status"]["satisfied"], true);
    // Declaration-only policy removal is a normal reviewed change; it does not relaunch.
    let mut remove_policy = request(&d, 3, bundle("second", false)).await;
    remove_policy.options.rollout = initial.options.rollout.clone();
    let p = preview(&d, &mut remove_policy).await;
    assert_eq!(p.changes[manual], "changed");
    assert!(p.effects[manual].contains("launch unchanged"));
    assert_ne!(p.digest, change_digest);
    d.client
        .post::<_, Value>("/v1/sets/apply", &remove_policy)
        .await
        .unwrap();
    reconciler.reconcile_once().unwrap();
    assert_eq!(runtime.starts.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_publication_preserves_suspension_and_requires_resume_before_rollout() {
    let root = tempfile::tempdir().unwrap();
    let anchor = Arc::new(MemberKey::generate().unwrap().0);
    let d = daemon(root.path(), "amber", anchor.clone(), &anchor).await;
    append(
        &d.store,
        "daemon/amber",
        "daemon.started",
        json!({"status":"running", "features":{"owned_sets":1,"seat_rollout":1,"seat_rollout_manual":1}}),
    );
    let subject = "agent/garden/orchard";
    let workspace = root.path().join("orchard");
    std::fs::create_dir_all(&workspace).unwrap();
    let bundle = |model: &str| {
        format!("version 2\nagent \"garden/orchard\" {{ host \"amber\"; workspace {:?}; rollout \"manual\"; harness \"claude\" {{ model {model:?}; }} }}\n",
            workspace.display().to_string())
    };
    let mut initial = request(&d, 1, bundle("first")).await;
    preview(&d, &mut initial).await;
    d.client.post::<_, Value>("/v1/sets/apply", &initial).await.unwrap();
    let incumbent = d.store.selected_desired_token(subject).unwrap().unwrap();
    let member = d.store.desired_subject_with_writer(subject).unwrap().unwrap().0.member.unwrap();
    append(&d.store, subject, "runtime.action.succeeded",
        json!({"action":"start","desired_token":incumbent,"incarnation_id":"original"}));
    append(&d.store, subject, "runtime.observed",
        json!({"status":"running","host":"amber","runtime_id":member.runtime_id,"terminal":true,"incarnation_id":"original"}));
    native_observation(&d.store, subject, "original");
    d.client.post::<_, Value>("/v1/agents/suspend",
        &json!({"subject":subject,"actor":"person/operator","idempotency_key":"suspend-before-publication"}))
        .await.unwrap();
    let operation = st3::suspension::current(&d.store, subject).unwrap().unwrap();
    assert_eq!(operation.phase, "quiescing");

    // Publish while suspend is in flight. Owner acknowledgements model the stopped
    // runtime boundary; native process continuity is covered by the lifecycle tests.
    let mut changed = request(&d, 2, bundle("second")).await;
    preview(&d, &mut changed).await;
    d.client.post::<_, Value>("/v1/sets/apply", &changed).await.unwrap();
    assert_eq!(st3::suspension::current(&d.store, subject).unwrap().unwrap().operation_id, operation.operation_id);
    for (key, phase) in [
        (st3::suspension::suspend_snapshot_key(&operation.operation_id), "snapshotting"),
        (st3::suspension::suspend_completed_key(&operation.operation_id), "suspended"),
    ] {
        d.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.succeeded".into(),
            actor: Some("person/operator".into()),
            fields: serde_json::from_value(json!({"action":"suspend","operation_status":phase,"harness":"claude","native_session_id":"native-agent-garden-orchard"})).unwrap(),
            evidence: vec![operation.operation_id.clone()],
            expected_subject: None,
            idempotency_key: Some(key),
        }).unwrap();
    }
    append(&d.store, subject, "runtime.observed",
        json!({"status":"exited","host":"amber","runtime_id":member.runtime_id,"terminal":true,"incarnation_id":"original","exit_code":0}));
    let runtime = Arc::new(RolloutRuntime::default());
    let reconciler = st3::reconcile::Reconciler::new(
        d.store.clone(), runtime.clone(), "amber".into(), Arc::new(Notify::new()));
    reconciler.reconcile_once().unwrap();
    assert!(runtime.starts.lock().unwrap().is_empty());
    let shown: Value = d.client.get(&format!("/v1/client/agents/{subject}")).await.unwrap();
    assert_eq!(shown["state"], "suspended");
    assert_eq!(shown["rollout"]["mode"], "manual");
    assert_eq!(shown["rollout"]["status"], "published, rollout pending (manual)");
    let refused = d.client.post::<_, Value>("/v1/agents/rollout",
        &json!({"subject":subject,"actor":"person/operator","expected_desired":d.store.selected_desired_token(subject).unwrap().unwrap(),"expected_incarnation":"original","policy":st3::rollout::Policy::when_idle(1_800_000, false),"idempotency_key":"rollout-while-suspended"}))
        .await.unwrap_err();
    assert!(refused.to_string().contains("rollout-suspended"), "{refused}");
    assert!(d.store.rollout(subject).unwrap().is_none());
    d.client.post::<_, Value>("/v1/agents/resume",
        &json!({"subject":subject,"actor":"person/operator","idempotency_key":"resume-incumbent"}))
        .await.unwrap();
    let resumed = st3::suspension::current(&d.store, subject).unwrap().unwrap();
    assert_eq!(resumed.phase, "restoring");
    assert_eq!(resumed.native_session_id.as_deref(), Some("native-agent-garden-orchard"));
    assert_ne!(d.store.selected_desired_token(subject).unwrap().unwrap(), incumbent);
    assert!(d.store.rollout(subject).unwrap().is_none());
    d.store.append_claim(&ClaimInput {
        subject: subject.into(),
        kind: "runtime.action.succeeded".into(),
        actor: Some("person/operator".into()),
        fields: serde_json::from_value(json!({"action":"resume","operation_status":"resumed","harness":"claude","native_session_id":"native-agent-garden-orchard","incarnation_id":"resumed-incumbent"})).unwrap(),
        evidence: vec![resumed.operation_id.clone()],
        expected_subject: None,
        idempotency_key: Some(st3::suspension::resume_completed_key(&resumed.operation_id)),
    }).unwrap();
    append(&d.store, subject, "runtime.action.succeeded",
        json!({"action":"start","desired_token":incumbent,"incarnation_id":"resumed-incumbent"}));
    append(&d.store, subject, "runtime.observed",
        json!({"status":"running","host":"amber","runtime_id":member.runtime_id,"terminal":true,"incarnation_id":"resumed-incumbent"}));
    native_observation(&d.store, subject, "resumed-incumbent");
    let shown: Value = d.client.get(&format!("/v1/client/agents/{subject}")).await.unwrap();
    assert_eq!(shown["state"], "running");
    assert_eq!(shown["rollout"]["status"], "published, rollout pending (manual)");
    d.client.post::<_, Value>("/v1/agents/rollout",
        &json!({"subject":subject,"actor":"person/operator","expected_desired":d.store.selected_desired_token(subject).unwrap().unwrap(),"expected_incarnation":"resumed-incumbent","policy":st3::rollout::Policy::when_idle(1_800_000, false),"idempotency_key":"rollout-after-resume"}))
        .await.unwrap();
    assert_eq!(d.store.rollout(subject).unwrap().unwrap().phase, "draining");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_declaration_diffs_match_publication_reads_and_mission_previews() {
    use st3::model::{MissionRequest, MissionResponse};
    let root = tempfile::tempdir().unwrap();
    let key = Arc::new(MemberKey::generate().unwrap().0);
    let d = daemon(root.path(), "amber", key.clone(), &key).await;
    let source = |note: &str, interval: &str| {
        format!(
            r#"version 2
agent "garden/orchard" {{ command "true"; description "{note}"; env {{ TOKEN "fixture-value"; }} }}
mission "garden/harvest" state="ready" timeout="1m" {{
    goal "{note}"
    step "inspect" {{ agentless }}
}}
schedule "garden/daily" {{
    host "amber"
    every "{interval}"
    anchor "2026-01-01T00:00:00Z"
    work {{ mission "garden/harvest"; workspace "/tmp"; }}
}}
"#
        )
    };
    let mut schema: Value = serde_json::from_str(include_str!(
        "../../../docs/st3/client-v0/schemas/client-v0.schema.json"
    ))
    .unwrap();
    schema["oneOf"] = json!([{ "$ref": "#/$defs/PublicationDefinition" }]);
    let validator = jsonschema::options().build(&schema).unwrap();
    let mut first = request(&d, 1, source("first", "6h")).await;
    let p = preview(&d, &mut first).await;
    assert_eq!(p.declaration_diffs.len(), 3);
    assert!(
        p.declaration_diffs
            .values()
            .all(|diff| diff.before.is_none())
    );
    assert_eq!(
        p.declaration_diffs["mission/garden/harvest"].after["max_active_runs"],
        1
    );
    assert_eq!(
        p.declaration_diffs["mission/garden/harvest"].after["revision_cutover"],
        "restart-active"
    );
    let mission = &p.declaration_diffs["mission/garden/harvest"].after;
    assert_eq!(mission["timeout_ms"], 60_000);
    assert_eq!(
        mission["steps"]["inspect"]["retry"],
        json!({"attempts": 1, "backoff_ms": 0})
    );
    d.client
        .post::<_, Value>("/v1/sets/apply", &first)
        .await
        .unwrap();
    let read = |subject: &str| {
        format!(
            "/v1/client/publication-definition?subject={}",
            urlencoding::encode(subject)
        )
    };
    for (subject, diff) in &p.declaration_diffs {
        let response: Value = d.client.get(&read(subject)).await.unwrap();
        assert!(
            validator.is_valid(&response),
            "{:?}",
            validator
                .iter_errors(&response)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(response["declaration"], diff.after);
        assert_eq!(response["subject"], *subject);
    }
    let mut updated = request(&d, 2, source("second", "12h")).await;
    let changed = preview(&d, &mut updated).await;
    let ordinary: MissionResponse = d
        .client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: IntentInput {
                    kdl: updated
                        .intent
                        .kdl
                        .split("\nschedule ")
                        .next()
                        .unwrap()
                        .into(),
                    source_name: None,
                },
                at_index: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(ordinary.declaration_diffs.len(), 2);
    for (subject, diff) in &ordinary.declaration_diffs {
        assert_eq!(
            serde_json::to_value(diff).unwrap(),
            serde_json::to_value(&changed.declaration_diffs[subject]).unwrap()
        );
    }
    assert!(
        changed.declaration_diffs["mission/garden/harvest"]
            .fields
            .contains("/goals")
    );
    for (subject, diff) in &changed.declaration_diffs {
        assert_eq!(
            diff.before.as_ref(),
            Some(&p.declaration_diffs[subject].after)
        );
        assert!(!diff.fields.is_empty());
    }
    d.client
        .post::<_, Value>("/v1/sets/apply", &updated)
        .await
        .unwrap();
    for (subject, diff) in &changed.declaration_diffs {
        let response: Value = d.client.get(&read(subject)).await.unwrap();
        assert!(
            validator.is_valid(&response),
            "{:?}",
            validator
                .iter_errors(&response)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(response["declaration"], diff.after);
    }
    let mut same = request(&d, 3, source("second", "12h")).await;
    assert!(preview(&d, &mut same).await.declaration_diffs.is_empty());
    let mut empty = request(&d, 3, "version 2\n".into()).await;
    empty.options.allow_empty = true;
    let retire = preview(&d, &mut empty).await;
    assert_eq!(retire.declaration_diffs.len(), 3);
    assert_eq!(
        retire.declaration_diffs["mission/garden/harvest"].after["state"],
        "retired"
    );
    empty.options.confirm_retire = Some(retire.digest.clone());
    d.client
        .post::<_, Value>("/v1/sets/apply", &empty)
        .await
        .unwrap();
    for (subject, diff) in &retire.declaration_diffs {
        let response: Value = d.client.get(&read(subject)).await.unwrap();
        assert!(
            validator.is_valid(&response),
            "{:?}",
            validator
                .iter_errors(&response)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(response["declaration"], diff.after);
        assert_eq!(
            diff.before.as_ref(),
            Some(&changed.declaration_diffs[subject].after)
        );
    }
    let mut restored = request(&d, 4, source("second", "12h")).await;
    let restore = preview(&d, &mut restored).await;
    for (subject, diff) in &restore.declaration_diffs {
        assert_eq!(restore.changes[subject], "added");
        assert_eq!(
            diff.before.as_ref(),
            Some(&retire.declaration_diffs[subject].after)
        );
        assert_eq!(diff.after, changed.declaration_diffs[subject].after);
    }
    // Old serialized previews still deserialize with an empty additive diff map.
    let mut legacy = serde_json::to_value(retire).unwrap();
    legacy.as_object_mut().unwrap().remove("declaration_diffs");
    assert!(
        serde_json::from_value::<Preview>(legacy)
            .unwrap()
            .declaration_diffs
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_diff_reads_existing_unmanaged_definition_and_invalid_actors_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let key = Arc::new(MemberKey::generate().unwrap().0);
    let d = daemon(root.path(), "amber", key.clone(), &key).await;
    let initial = st3::graph::parse_intent(&bundle("unmanaged", false), "amber").unwrap();
    d.store
        .apply_internal(&initial, "unmanaged-fixture")
        .unwrap();
    let mut adopt = request(&d, 1, bundle("managed", false)).await;
    adopt.options.adopt.insert("agent/garden/orchard".into());
    let p = preview(&d, &mut adopt).await;
    let before = &p.declaration_diffs["agent/garden/orchard"].before;
    assert_eq!(
        before.as_ref().unwrap(),
        &serde_json::to_value(&initial.subjects["agent/garden/orchard"]).unwrap()
    );
    adopt.actor = "daemon/not-an-actor".into();
    assert!(
        d.client
            .post::<_, Value>("/v1/sets/preview", &adopt)
            .await
            .is_err()
    );
}
