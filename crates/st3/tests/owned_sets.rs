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
            let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"));
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
            std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
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
