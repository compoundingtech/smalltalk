#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use st3::api::AppState;
use st3::model::{AttentionRequest, ClaimInput, IntentInput, MissionRunRequest};
use st3::store::Store;
use tokio::sync::{Notify, watch};

fn test_state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("client-v0-cli").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "client-v0-cli".into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec!["offline-peer".into()],
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

async fn run_cli(socket: &Path, args: &[&str]) -> Output {
    run_cli_mode(socket, true, args).await
}

async fn run_cli_human(socket: &Path, args: &[&str]) -> Output {
    run_cli_mode(socket, false, args).await
}

async fn run_cli_mode(socket: &Path, json: bool, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let socket = socket.to_path_buf();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(binary);
        command.arg("--endpoint").arg(socket);
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    })
    .await
    .unwrap()
}

async fn run_cli_with_agent_env(socket: &Path, agent: &str, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3").to_path_buf();
    let socket = socket.to_path_buf();
    let agent = agent.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .arg("--endpoint")
            .arg(socket)
            .arg("--json")
            .args(args)
            .env("ST_AGENT", agent)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

fn value(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "decode CLI JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn service_permissions_honors_global_json_flag() {
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("st3"))
        .args(["--json", "service", "permissions"])
        .output()
        .unwrap();
    let response = value(&output);
    assert!(response["platform"].is_string(), "{response}");
    assert!(response["guidance"].is_string(), "{response}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_list_does_not_return_an_empty_page_with_more_managed_sessions() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    for name in ["first", "second"] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("agent/import-{name}"),
                kind: "runtime.observed".into(),
                actor: Some(format!("agent/import-{name}")),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String(format!("import-{name}"))),
                    (
                        "incarnation_id".into(),
                        Value::String(format!("import-{name}:i1")),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("reachability".into(), Value::String("local".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());

    let page = value(&run_cli(&socket, &["import", "ls", "--limit", "1"]).await);
    assert!(page["value"]["items"].as_array().unwrap().is_empty());
    assert_eq!(page["value"]["page"]["has_more"], false);
    assert!(page["value"]["page"]["next_cursor"].is_null());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversations_cli_handles_multiple_message_pages_and_exact_reads() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    for index in 0..205 {
        let mut fields = BTreeMap::from([
            ("from".into(), Value::String("agent/sender".into())),
            ("to".into(), Value::String("agent/receiver".into())),
            ("content".into(), Value::String(format!("body {index}"))),
            ("status".into(), Value::String("sent".into())),
        ]);
        if index == 204 {
            fields.insert(
                "in_reply_to".into(),
                Value::String("message/page-000".into()),
            );
        }
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("message/page-{index:03}"),
                kind: "message.sent".into(),
                actor: Some("agent/sender".into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());

    let listed = value(&run_cli(&socket, &["conversations", "ls", "agent/receiver"]).await);
    assert_eq!(listed.as_array().unwrap().len(), 205);
    let sender_copy = value(
        &run_cli(
            &socket,
            &[
                "conversations",
                "read",
                "message/page-203",
                "--as",
                "agent/sender",
            ],
        )
        .await,
    );
    assert_eq!(sender_copy["status"], "sent");
    let sender_archive = run_cli(
        &socket,
        &[
            "conversations",
            "read",
            "message/page-203",
            "--as",
            "agent/sender",
            "--archive",
        ],
    )
    .await;
    assert!(!sender_archive.status.success());
    assert_eq!(
        value(&run_cli(&socket, &["conversations", "thread", "message/page-203"]).await)[0]["status"],
        "sent"
    );
    let thread = value(&run_cli(&socket, &["conversations", "thread", "message/page-204"]).await);
    assert_eq!(thread.as_array().unwrap().len(), 2);
    let read = value(
        &run_cli(
            &socket,
            &[
                "conversations",
                "read",
                "message/page-204",
                "--as",
                "agent/receiver",
            ],
        )
        .await,
    );
    assert_eq!(read["subject"], "message/page-204");
    let export_dir = root.path().join("export");
    std::fs::create_dir_all(export_dir.join("receiver/inbox")).unwrap();
    let stale = export_dir.join("receiver/inbox/stale.md");
    std::fs::write(&stale, "old projection").unwrap();
    let exported = value(
        &run_cli(
            &socket,
            &["conversations", "export", export_dir.to_str().unwrap()],
        )
        .await,
    );
    assert_eq!(exported["messages"], 205);
    assert!(!stale.exists());
    assert_eq!(
        std::fs::read_dir(export_dir.join("receiver/inbox"))
            .unwrap()
            .count(),
        205
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attention_withdraw_removes_an_obsolete_request_from_now() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let request = state
        .store
        .request_attention(
            "attention/obsolete",
            &AttentionRequest {
                reviewer: "person/nathan".into(),
                title: "No action needed".into(),
                reason: "The original blocker has cleared.".into(),
                severity: "warning".into(),
                targets: Vec::new(),
                actor: "agent/typecase/worker".into(),
                idempotency_key: "obsolete-request".into(),
            },
        )
        .unwrap();
    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());
    let withdrawn = value(
        &run_cli(
            &socket,
            &[
                "attention",
                "withdraw",
                &request.subject,
                "--reason",
                "No action needed now",
                "--as",
                "agent/typecase/worker",
            ],
        )
        .await,
    );
    assert_eq!(withdrawn["status"], "withdrawn");
    let current = value(&run_cli(&socket, &["attention", "ls", "--as", "person/nathan"]).await);
    assert!(current["value"]["items"].as_array().unwrap().is_empty());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_product_cli_uses_real_client_v0_envelopes_and_fences() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/cli-runtime".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/cli-runtime".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("cli-runtime".into())),
                (
                    "incarnation_id".into(),
                    Value::String("cli-runtime:i1".into()),
                ),
                ("status".into(), Value::String("running".into())),
                ("reachability".into(), Value::String("local".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let source = r#"version 2
mission "cli/root" state="ready" {
  goal "Expose nested work."
  step "child" { agentless }
}
mission "cli/child" state="ready" {
  goal "Prove list and detail use the same replicated projection."
  agent "remote" { workspace "/tmp"; command "true"; restart "never" }
  step "nested" {
    assigned-to "agent/${ST_MISSION_RUN}/remote"
    goal "Remain visible through client-v0."
  }
}
"#;
    let intent = st3::graph::parse_intent(source, "client-v0-cli").unwrap();
    let planned = store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, "cli-nested-work-source")
        .unwrap();
    let root_run = store
        .create_mission_run(&MissionRunRequest {
            mission: "cli/root".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/nathan".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cli-root-run".into(),
        })
        .unwrap();
    let child_run = store
        .create_child_mission_run(
            &MissionRunRequest {
                mission: "cli/child".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/nathan".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "cli-child-run".into(),
            },
            &root_run,
            &root_run.steps[0].subject,
            None,
        )
        .unwrap();
    let nested_work = child_run.steps[0].subject.clone();
    store.set_step_state(&nested_work, "ready", None).unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "host/discovered-history".into(),
            kind: "transport.observed".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("status".into(), Value::String("up".into())),
                ("protocol".into(), Value::String("fabric-loopback".into())),
                ("last_success_at".into(), Value::from(1_u64)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/cli-stopped".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/cli-stopped".into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("cli-stopped".into())),
                (
                    "incarnation_id".into(),
                    Value::String("cli-stopped:i1".into()),
                ),
                ("status".into(), Value::String("stopped".into())),
                ("reachability".into(), Value::String("local".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "custom/client/cli-device".into(),
            kind: "custom.client.pairing-completed".into(),
            actor: Some("person/nathan".into()),
            fields: BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(b"cli credential"))),
                ),
                ("device_id".into(), Value::String("device/cli".into())),
                ("person_id".into(), Value::String("person/nathan".into())),
                (
                    "session_actor".into(),
                    Value::String("person/nathan/session/cli".into()),
                ),
                ("scopes".into(), serde_json::json!(["read.projections"])),
                ("expires_at_unix_ms".into(), serde_json::json!(u64::MAX / 2)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    let server_socket = socket.clone();
    let server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_socket, st3::api::router(state)).await },
        );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists(), "client-v0 test socket did not appear");

    let now = value(&run_cli(&socket, &["now", "--as", "person/nathan"]).await);
    assert_eq!(now["api_version"], "st3.client.v0");
    assert_eq!(now["value"]["collection"], "now");
    assert_eq!(now["value"]["filters"]["person"], "person/nathan");

    for arguments in [
        vec!["attention", "ls", "--as", "person/nathan"],
        vec!["agents", "ls"],
        vec!["agents", "tree"],
        vec!["work", "ls"],
        vec!["terminals", "ls"],
    ] {
        let page = value(&run_cli(&socket, &arguments).await);
        assert_eq!(page["api_version"], "st3.client.v0", "{arguments:?}");
        assert_eq!(page["value"]["kind"], "page", "{arguments:?}");
        assert!(page["value"]["items"].is_array(), "{arguments:?}");
        assert!(page["value"]["filters"].is_object(), "{arguments:?}");
    }

    let work = value(&run_cli(&socket, &["work", "ls"]).await);
    let listed = work["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == nested_work)
        .expect("the nested remote-owned step is listed");
    assert_eq!(listed["mission_run_id"], child_run.subject);
    let detail = value(&run_cli(&socket, &["work", "show", &nested_work]).await);
    assert_eq!(detail["value"]["id"], nested_work);
    assert_eq!(detail["value"]["mission_run_id"], child_run.subject);
    let human_detail = run_cli_human(&socket, &["work", "show", &nested_work]).await;
    assert!(human_detail.status.success());
    let human_detail = String::from_utf8(human_detail.stdout).unwrap();
    assert!(human_detail.starts_with(&format!("WORK  {nested_work}")));
    assert!(human_detail.contains("Goal: Remain visible through client-v0."));
    let inherited = value(
        &run_cli_with_agent_env(&socket, "agent/ambient.must-not-filter", &["work", "ls"]).await,
    );
    assert_eq!(inherited["value"]["filters"], serde_json::json!({}));
    assert!(
        inherited["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == nested_work),
        "an inherited ST_AGENT must not silently filter a human work inventory"
    );

    let first_agent = value(&run_cli(&socket, &["agents", "ls", "--all", "--limit", "1"]).await);
    assert_eq!(first_agent["value"]["page"]["limit"], 1);
    assert!(first_agent["value"]["page"]["next_cursor"].is_string());

    let machines = value(&run_cli(&socket, &["machines"]).await);
    assert_eq!(machines["api_version"], "st3.client.v0");
    assert_eq!(machines["value"]["items"][0]["id"], "machine/client-v0-cli");
    assert_eq!(machines["value"]["items"][0]["kind"], "machine");
    assert_eq!(
        machines["value"]["items"][0]["runtime_ids"][0],
        "runtime/cli-runtime"
    );
    assert_eq!(
        machines["value"]["items"][0]["capacity"]["state"],
        "unknown"
    );
    assert_eq!(
        machines["value"]["items"][0]["occupancy"]["running_runtimes"],
        1
    );
    assert!(
        machines["value"]["items"][0]["projects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(machines["value"]["items"][1]["id"], "machine/offline-peer");
    assert_eq!(machines["value"]["items"][1]["state"], "indeterminate");
    assert!(
        machines["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "machine/discovered-history")
    );
    let local_revision = machines["value"]["items"][0]["revision"].clone();
    let local_updated_at = machines["value"]["items"][0]["updated_at"].clone();
    store
        .append_claim(&ClaimInput {
            subject: "message/unrelated-machine-revision".into(),
            kind: "message.sent".into(),
            actor: Some("agent/sender".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("agent/sender".into())),
                ("to".into(), Value::String("agent/recipient".into())),
                ("content".into(), Value::String("unrelated".into())),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::Null),
                ("in_reply_to".into(), Value::Null),
                ("tags".into(), Value::Array(Vec::new())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let unchanged_machines = value(&run_cli(&socket, &["machines"]).await);
    assert_eq!(
        unchanged_machines["value"]["items"],
        machines["value"]["items"]
    );
    assert_eq!(
        unchanged_machines["value"]["items"][0]["revision"],
        local_revision
    );
    assert_eq!(
        unchanged_machines["value"]["items"][0]["updated_at"],
        local_updated_at
    );
    let machine_history = value(&run_cli(&socket, &["machines", "--all"]).await);
    assert_eq!(
        machine_history["value"]["items"][0]["runtime_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        machine_history["value"]["items"][0]["occupancy"]["running_runtimes"],
        1
    );
    let discovered = machine_history["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "machine/discovered-history")
        .unwrap();
    assert_eq!(discovered["operational"]["layer"], "history");
    assert_eq!(discovered["operational"]["actionable"], false);
    assert!(
        discovered["operational"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason == "discovered-history")
    );

    let activity = value(&run_cli(&socket, &["activity", "--limit", "10"]).await);
    assert_eq!(activity["api_version"], "st3.client.v0");
    assert_eq!(activity["value"]["kind"], "event-page");

    let devices = value(&run_cli(&socket, &["devices", "--as", "person/nathan"]).await);
    assert_eq!(devices["value"]["items"][0]["id"], "device/cli");
    assert_eq!(devices["value"]["items"][0]["state"], "active");

    let pairing = value(
        &run_cli(
            &socket,
            &["devices", "--as", "person/nathan", "pair", "test-phone"],
        )
        .await,
    );
    assert_eq!(pairing["api_version"], "st3.client.v0");
    assert_eq!(pairing["value"]["kind"], "pairing-challenge");

    let revoked = value(
        &run_cli(
            &socket,
            &["devices", "--as", "person/nathan", "revoke", "device/cli"],
        )
        .await,
    );
    assert_eq!(revoked["api_version"], "st3.client.v0");
    assert_eq!(revoked["value"]["status"], "completed");

    let current = value(&run_cli(&socket, &["devices", "--as", "person/nathan"]).await);
    assert!(current["value"]["items"].as_array().unwrap().is_empty());
    let history = value(&run_cli(&socket, &["devices", "--as", "person/nathan", "--all"]).await);
    assert_eq!(history["value"]["items"][0]["state"], "revoked");

    for (arguments, heading) in [
        (vec!["now", "--as", "person/nathan"], "NEEDS YOU"),
        (vec!["machines"], "MACHINES"),
        (vec!["activity", "--limit", "10"], "ACTIVITY"),
        (vec!["devices", "--as", "person/nathan", "--all"], "DEVICES"),
        (
            vec!["attention", "ls", "--as", "person/nathan"],
            "HUMAN ATTENTION",
        ),
        (vec!["agents", "ls"], "AGENTS"),
        (vec!["agents", "tree"], "AGENT TREE"),
        (vec!["work", "ls"], "WORK"),
        (vec!["terminals", "ls"], "TERMINALS"),
    ] {
        let output = run_cli_human(&socket, &arguments).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rendered = String::from_utf8(output.stdout).unwrap();
        assert!(rendered.starts_with(heading), "{rendered}");
        assert!(!rendered.contains("api_version"), "{rendered}");
        assert!(!rendered.contains("request_id"), "{rendered}");
        assert!(!rendered.contains("snapshot"), "{rendered}");
    }

    server.abort();
}
