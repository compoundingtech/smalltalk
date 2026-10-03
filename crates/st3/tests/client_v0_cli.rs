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
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let socket = socket.to_path_buf();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut command = st3::test_support::command(binary);
        // These are operator commands. A harness running the suite must not lend its own seat
        // identity, which scopes `work ls` and fences agent actors.
        command
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .arg("--endpoint")
            .arg(socket);
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    })
    .await
    .unwrap()
}

async fn run_cli_with_agent_env(socket: &Path, agent: &str, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let socket = socket.to_path_buf();
    let agent = agent.to_owned();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        st3::test_support::command(binary)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seat_bootstraps_cancels_and_stops_with_its_own_cli_identity() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
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
    let parent = root.path().join("coordinator.kdl");
    std::fs::write(
        &parent,
        r#"version 2
agent "example/operations/coordinator" {
  workspace "."
  command "true"
}
"#,
    )
    .unwrap();
    value(
        &run_cli(
            &socket,
            &[
                "agents",
                "apply",
                parent.to_str().unwrap(),
                "--as",
                "person/operator",
            ],
        )
        .await,
    );
    let actor = "agent/example/operations/coordinator";
    let deputy = root.path().join("deputy.kdl");
    std::fs::write(
        &deputy,
        r#"version 2
agent "example/operations/deputy" {
  workspace "."
  command "true"
  mission-authority { cancel "example/jobs/docs/*" }
}
"#,
    )
    .unwrap();
    // Free mode: the seat declares another seat with no grant, and st says it ignores the
    // deputy's authority block.
    let applied = run_cli_with_agent_env(
        &socket,
        actor,
        &["agents", "apply", deputy.to_str().unwrap(), "--as", actor],
    )
    .await;
    value(&applied);
    assert!(
        String::from_utf8_lossy(&applied.stderr).contains(
            "`agent/example/operations/deputy` declares `mission-authority`, which st ignores"
        ),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let mission = root.path().join("mission.kdl");
    std::fs::write(&mission, "version 2\nmission \"example/jobs/docs/one\" state=\"ready\" { goal \"Do the assigned work.\"; step \"wait\" { agentless } }\n").unwrap();
    value(
        &run_cli_with_agent_env(
            &socket,
            actor,
            &[
                "missions",
                "publish",
                mission.to_str().unwrap(),
                "--as",
                actor,
            ],
        )
        .await,
    );
    value(
        &run_cli_with_agent_env(
            &socket,
            actor,
            &[
                "missions",
                "start",
                "example/jobs/docs/one",
                "--id",
                "example/jobs/docs/one/run",
                "--workspace",
                root.path().to_str().unwrap(),
                "--as",
                actor,
            ],
        )
        .await,
    );
    let deputy_actor = "agent/example/operations/deputy";
    value(
        &run_cli_with_agent_env(
            &socket,
            deputy_actor,
            &[
                "missions",
                "cancel",
                "mission-run/example/jobs/docs/one/run",
                "--reason",
                "The work was superseded.",
                "--as",
                deputy_actor,
            ],
        )
        .await,
    );
    assert_eq!(
        store
            .mission_run("example/jobs/docs/one/run")
            .unwrap()
            .unwrap()
            .phase,
        "cleanup-cancelled"
    );
    let run = store
        .mission_run("example/jobs/docs/one/run")
        .unwrap()
        .unwrap();
    assert_eq!(run.requester, actor, "the agent started the run as itself");
    value(
        &run_cli_with_agent_env(
            &socket,
            actor,
            &["agents", "stop", deputy_actor, "--as", actor],
        )
        .await,
    );
    // A seat stops itself.
    value(&run_cli_with_agent_env(&socket, actor, &["agents", "stop", actor, "--as", actor]).await);
    let stopped = store
        .desired_subjects()
        .unwrap()
        .into_iter()
        .filter(|desired| desired.kind == "stop")
        .map(|desired| desired.subject)
        .collect::<Vec<_>>();
    assert!(stopped.contains(&actor.to_owned()), "{stopped:?}");
    assert!(stopped.contains(&deputy_actor.to_owned()), "{stopped:?}");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operational_cli_lists_outcomes_summarizes_runs_and_reports_performance() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let source = "version 2\nmission \"example/operations\" state=\"ready\" {\n concurrent-runs max=20\n goal \"Build the example.\"\n step \"build\" {goal \"Build.\"}\n}\n";
    let intent = st3::graph::parse_intent(source, "client-v0-cli").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "operations")
        .unwrap();
    for i in 0..16 {
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "example/operations".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: format!("operations-{i}"),
            })
            .unwrap();
        if i < 2 {
            state
                .store
                .set_step_state(
                    &run.steps[0].subject,
                    "failed",
                    Some("the active execution timeout expired"),
                )
                .unwrap();
            // Runtime cleanup may write the terminal state without repeating the original reason.
            state
                .store
                .set_mission_run_state(
                    &run.id,
                    "running",
                    "cleanup-failed",
                    Some("the mission timeout expired"),
                )
                .unwrap();
            state
                .store
                .set_mission_run_state(&run.id, "failed", "terminal", None)
                .unwrap();
        }
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
    let overview =
        value(&run_cli(&socket, &["missions", "show", "mission/example/operations"]).await);
    assert_eq!(overview["total_runs"], 16);
    assert_eq!(overview["counts"]["running"], 14);
    assert_eq!(overview["counts"]["failed"], 2);
    let shown = run_cli_human(&socket, &["missions", "show", "mission/example/operations"]).await;
    assert!(shown.status.success());
    assert!(String::from_utf8_lossy(&shown.stdout).contains("running: 14"));
    let listing = value(&run_cli(&socket, &["missions", "ls", "--all"]).await);
    assert_eq!(listing["value"]["items"][0]["total_runs"], 16);
    for collection in ["missions", "work"] {
        let page = value(
            &run_cli(
                &socket,
                &[
                    collection, "ls", "--since", "6h", "--status", "failed", "--limit", "1",
                ],
            )
            .await,
        );
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert!(
            page["items"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("timeout")
        );
        let next = page["next_cursor"].as_str().unwrap();
        let next_page = value(
            &run_cli(
                &socket,
                &[collection, "ls", "--limit", "1", "--cursor", next],
            )
            .await,
        );
        assert_ne!(next_page["items"][0]["id"], page["items"][0]["id"]);
        let timed = value(
            &run_cli(
                &socket,
                &[collection, "ls", "--since", "6h", "--status", "timed-out"],
            )
            .await,
        );
        assert_eq!(timed["items"].as_array().unwrap().len(), 2);
        let empty = value(
            &run_cli(
                &socket,
                &[
                    collection,
                    "ls",
                    "--since",
                    "2020-01-01T00:00:00Z",
                    "--until",
                    "2020-01-02T00:00:00Z",
                    "--status",
                    "cancelled",
                ],
            )
            .await,
        );
        assert!(empty["items"].as_array().unwrap().is_empty());
    }
    let report = value(&run_cli(&socket, &["doctor", "--performance"]).await);
    assert_eq!(report["window_seconds"], 300);
    assert!(!report["requests"].as_array().unwrap().is_empty());
    assert!(!report["queries"].as_array().unwrap().is_empty());
    // Each request is counted under the command that sent it.
    assert!(report["request_count"].as_u64().unwrap() > 0);
    assert!(
        report["client_requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |row| row["client"].as_str().unwrap().contains("st3 missions")
                    && row["count"].as_u64().unwrap() > 0
            ),
        "{report:#}"
    );
    let shown = run_cli_human(&socket, &["doctor", "--performance"]).await;
    assert!(shown.status.success());
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.contains("REQUESTS AND TASKS"));
    assert!(shown.contains("REQUESTS BY CLIENT"), "{shown}");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_declaration_cli_redacts_environment_unless_explicitly_requested() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let source = "version 2\nagent \"read/test\" { workspace \"/tmp\"; command \"true\"; env { API_TOKEN \"private-value\" } }";
    let intent = st3::graph::parse_intent(source, "client-v0-cli").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    assert!(planned.blockers.is_empty(), "{:?}", planned.blockers);
    state
        .store
        .apply(&intent, &planned.subject_tokens, source)
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
    for show_values in [false, true] {
        let mut args = vec!["subject", "show", "agent/read/test", "--kdl"];
        if show_values {
            args.push("--show-env-values");
        }
        let output = run_cli_human(&socket, &args).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let kdl = String::from_utf8(output.stdout).unwrap();
        let parsed = st3::graph::parse_intent(&kdl, "client-v0-cli").unwrap();
        let desired = &parsed.subjects["agent/read/test"].desired;
        let env = desired["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["name"] == "env")
            .unwrap();
        assert_eq!(env["children"][0]["name"], "API_TOKEN");
        assert_eq!(
            env["children"][0]["arguments"][0],
            if show_values {
                "private-value"
            } else {
                "<redacted>"
            }
        );
        if !show_values {
            assert!(!kdl.contains("private-value"), "{kdl}");
        }
    }
    server.abort();
}

#[test]
fn service_permissions_honors_global_json_flag() {
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
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
async fn a_conversation_thread_pages_through_the_history_once() {
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
    let pages = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = pages.clone();
    let router = st3::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let counted = counted.clone();
            async move {
                if request.uri().path() == "/v1/messages/page" {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                next.run(request).await
            }
        },
    ));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, router).await });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());

    let thread = value(&run_cli(&socket, &["conversations", "thread", "message/page-204"]).await);
    let subjects = thread
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["subject"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(subjects, ["message/page-000", "message/page-204"]);
    // 205 messages are three pages of 100. Every page makes the daemon read each message.
    assert_eq!(
        pages.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "a thread must page through the message history once"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_attention_mutation_requires_source_migration() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let request = state
        .store
        .request_attention(
            "attention/obsolete",
            &AttentionRequest {
                reviewer: "person/alex".into(),
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
    let withdrawn = run_cli(
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
    .await;
    assert!(!withdrawn.status.success());
    assert!(String::from_utf8_lossy(&withdrawn.stderr).contains("attention-migrated"));
    let current = value(&run_cli(&socket, &["attention", "ls", "--as", "person/alex"]).await);
    assert!(current["value"]["items"].as_array().unwrap().is_empty());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_person_ask_is_completed_by_its_assigned_person() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        store.origin(),
    )
    .unwrap();
    store.apply_internal(&intent, "cli-person-asker").unwrap();
    let actor = format!("agent/{}.asker", store.origin());
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
    let ask_args = [
        "work",
        "ask",
        "--for",
        "person/avery",
        "--title",
        "Choose release date",
        "--reason",
        "The release needs a date",
        "--new-run",
        "release-date",
        "--as",
        &actor,
        "--idempotency-key",
        "cli-ask",
    ];
    let ask = value(&run_cli(&socket, &ask_args).await);
    let subject = ask["subject"].as_str().unwrap();
    assert_eq!(
        value(&run_cli(&socket, &ask_args).await)["subject"],
        subject
    );
    let listed = value(&run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await);
    assert_eq!(listed["value"]["items"][0]["source_id"], subject);
    assert_eq!(listed["value"]["items"][0]["attention_kind"], "person-step");
    let wrong = run_cli(
        &socket,
        &[
            "work",
            "done",
            subject,
            "--as",
            "person/robin",
            "--summary",
            "Friday",
        ],
    )
    .await;
    assert!(!wrong.status.success());
    let done = [
        "work",
        "done",
        subject,
        "--as",
        "person/avery",
        "--summary",
        "Friday",
        "--idempotency-key",
        "cli-done",
    ];
    assert_eq!(value(&run_cli(&socket, &done).await)["status"], "completed");
    assert_eq!(value(&run_cli(&socket, &done).await)["status"], "completed");
    let listed = value(&run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await);
    assert!(listed["value"]["items"].as_array().unwrap().is_empty());
    assert!(
        !store
            .events_after_bounded(0, 200)
            .unwrap()
            .iter()
            .any(|c| c.subject.starts_with("attention/"))
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_structured_choice_returns_the_selected_option_as_data() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
    let intent = st3::graph::parse_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        store.origin(),
    )
    .unwrap();
    store
        .apply_internal(&intent, "cli-structured-asker")
        .unwrap();
    let actor = format!("agent/{}.asker", store.origin());
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
    let request = root.path().join("request.json");
    std::fs::write(
        &request,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "type": "choice",
            "question": "How should an oversized missions tree load?",
            "why_person": "It changes what every client shows.",
            "recommendation": {"answer": "paged", "reason": "Nothing fails for a large fleet."},
            "answers": [
                {"id": "paged", "label": "Page it", "consequence": "Clients read pages with explicit metadata."},
                {"id": "limit", "label": "Fail past a limit", "consequence": "Large trees return an error."}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let ask = value(
        &run_cli(
            &socket,
            &[
                "work",
                "ask",
                "--for",
                "person/avery",
                "--title",
                "Oversized missions tree",
                "--request",
                request.to_str().unwrap(),
                "--new-run",
                "tree-size",
                "--as",
                &actor,
                "--idempotency-key",
                "tree-size",
            ],
        )
        .await,
    );
    let subject = ask["subject"].as_str().unwrap();
    // Without --reason, the person reads the request's question.
    assert_eq!(
        ask["goals"][0],
        "How should an oversized missions tree load?"
    );
    let listed = value(&run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await);
    let item = &listed["value"]["items"][0];
    assert_eq!(item["source_id"], subject);
    assert_eq!(item["request"]["type"], "choice");
    assert_eq!(item["request"]["answers"][0]["id"], "paged");
    let words = run_cli(
        &socket,
        &[
            "work",
            "done",
            subject,
            "--as",
            "person/avery",
            "--summary",
            "paged please",
        ],
    )
    .await;
    assert!(!words.status.success());
    assert!(String::from_utf8_lossy(&words.stderr).contains("answer-required"));
    let done = value(
        &run_cli(
            &socket,
            &[
                "work",
                "done",
                subject,
                "--as",
                "person/avery",
                "--answer",
                "paged",
            ],
        )
        .await,
    );
    assert_eq!(done["status"], "completed");
    let shown = value(&run_cli(&socket, &["work", "show", subject]).await);
    let answer = &shown["value"]["person_answers"][0];
    assert_eq!(answer["respondent"], "person/avery");
    assert_eq!(answer["summary"], "Page it");
    assert_eq!(
        answer["answer"],
        serde_json::json!({"type": "choice", "outcome": "selected", "id": "paged", "label": "Page it"})
    );
    let human = run_cli_human(&socket, &["work", "show", subject]).await;
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("Person answer: selected paged: Page it")
    );
    server.abort();
}

/// An update brings a person what they asked for: it reaches their home without stopping the
/// poster, only for the person's own work, and opening it reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_update_reaches_home_only_for_asked_work_and_opening_reads_it() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
    let intent = st3::graph::parse_intent(
        r#"version 2
agent "reporter" { workspace "/tmp"; command "true" }
mission "release-report" state="ready" {
  goal "Report on the release.";
  step "report" { assigned-to "agent/reporter"; goal "Report on the release."; }
}
"#,
        store.origin(),
    )
    .unwrap();
    store.apply_internal(&intent, "cli-update").unwrap();
    let run = store
        .create_mission_run(&st3::model::MissionRunRequest {
            mission: "release-report".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/avery".into()),
            mode: Some("run".into()),
            inputs: Default::default(),
            idempotency_key: "release-report".into(),
        })
        .unwrap();
    let actor = format!("agent/{}.reporter", store.origin());
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
    let update = |person: &'static str| {
        let socket = socket.clone();
        let actor = actor.clone();
        let run = run.subject.clone();
        async move {
            run_cli(
                &socket,
                &[
                    "work",
                    "update",
                    "--for",
                    person,
                    "--about",
                    &run,
                    "--title",
                    "Release notes are ready",
                    "--body",
                    "The notes cover all three fixes.",
                    "--as",
                    &actor,
                    "--idempotency-key",
                    "release-notes",
                ],
            )
            .await
        }
    };
    let refused = update("person/someone-else").await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("update-not-asked"));
    let posted = value(&update("person/avery").await);
    let subject = posted["subject"].as_str().unwrap().to_owned();

    let listed = value(&run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await);
    let item = &listed["value"]["items"][0];
    assert_eq!(item["source_id"], subject);
    assert_eq!(item["attention_kind"], "person-step");
    assert_eq!(item["title"], "Release notes are ready");
    // Not a request: clients that predate updates read it as a free-text card.
    assert!(item["request"].is_null(), "{item}");
    assert_eq!(item["update"]["about"], run.subject);
    assert_eq!(
        item["action_parameters"]["work.done"]["answer"],
        serde_json::json!({"id": "read"})
    );

    let opened = run_cli_human(
        &socket,
        &["attention", "show", &subject, "--as", "person/avery"],
    )
    .await;
    assert!(opened.status.success(), "{opened:?}");
    let text = String::from_utf8_lossy(&opened.stdout);
    assert!(text.contains("The notes cover all three fixes."), "{text}");
    assert!(
        text.contains(&format!("UPDATE  you asked in {}", run.subject)),
        "{text}"
    );
    assert!(
        text.contains("Read: this update has left your home."),
        "{text}"
    );
    let listed = value(&run_cli(&socket, &["attention", "ls", "--as", "person/avery"]).await);
    assert_eq!(listed["value"]["items"], serde_json::json!([]));
    let shown = value(&run_cli(&socket, &["work", "show", &subject]).await);
    assert_eq!(
        shown["value"]["person_answers"][0]["answer"],
        serde_json::json!({"type": "update", "outcome": "read"})
    );
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
            requester: Some("person/alex".into()),
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
                requester: Some("person/alex".into()),
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
            actor: Some("person/alex".into()),
            fields: BTreeMap::from([
                (
                    "credential_hash".into(),
                    Value::String(hex::encode(Sha256::digest(b"cli credential"))),
                ),
                ("device_id".into(), Value::String("device/cli".into())),
                ("person_id".into(), Value::String("person/alex".into())),
                (
                    "session_actor".into(),
                    Value::String("person/alex/session/cli".into()),
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

    let now = value(&run_cli(&socket, &["now", "--as", "person/alex"]).await);
    assert_eq!(now["api_version"], "st3.client.v0");
    assert_eq!(now["value"]["collection"], "now");
    assert_eq!(now["value"]["filters"]["person"], "person/alex");

    for arguments in [
        vec!["attention", "ls", "--as", "person/alex"],
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
    assert_eq!(machines["value"]["items"][1]["state"], "last-seen");
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

    let devices = value(&run_cli(&socket, &["devices", "--as", "person/alex"]).await);
    assert_eq!(devices["value"]["items"][0]["id"], "device/cli");
    assert_eq!(devices["value"]["items"][0]["state"], "active");

    let pairing = value(
        &run_cli(
            &socket,
            &["devices", "--as", "person/alex", "pair", "test-phone"],
        )
        .await,
    );
    assert_eq!(pairing["api_version"], "st3.client.v0");
    assert_eq!(pairing["value"]["kind"], "pairing-challenge");

    let revoked = value(
        &run_cli(
            &socket,
            &["devices", "--as", "person/alex", "revoke", "device/cli"],
        )
        .await,
    );
    assert_eq!(revoked["api_version"], "st3.client.v0");
    assert_eq!(revoked["value"]["status"], "completed");

    let current = value(&run_cli(&socket, &["devices", "--as", "person/alex"]).await);
    assert!(current["value"]["items"].as_array().unwrap().is_empty());
    let history = value(&run_cli(&socket, &["devices", "--as", "person/alex", "--all"]).await);
    assert_eq!(history["value"]["items"][0]["state"], "revoked");

    for (arguments, heading) in [
        (vec!["now", "--as", "person/alex"], "NEEDS YOU"),
        (vec!["machines"], "MACHINES"),
        (vec!["activity", "--limit", "10"], "ACTIVITY"),
        (vec!["devices", "--as", "person/alex", "--all"], "DEVICES"),
        (
            vec!["attention", "ls", "--as", "person/alex"],
            "HUMAN ATTENTION",
        ),
        (vec!["agents", "ls"], "AGENTS"),
        (vec!["agents", "tree"], "AGENT TREE"),
        (vec!["work", "ls"], "WORK"),
        (vec!["terminals", "ls"], "TERMINALS"),
        (vec!["conversations", "ls", "person/alex"], "MESSAGES"),
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

async fn run_queue_cli(socket: &Path, config_home: &Path, json: bool, args: &[&str]) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let socket = socket.to_path_buf();
    let config_home = config_home.to_path_buf();
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut command = st3::test_support::command(binary);
        // Queue operator commands use the fixture's person, not the invoking harness seat.
        command
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .env("XDG_CONFIG_HOME", config_home)
            .arg("--endpoint")
            .arg(socket);
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_queue_cli_shows_seat_order_and_records_person_and_agent_moves() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let config_home = root.path().join("config");
    std::fs::create_dir_all(config_home.join("st3")).unwrap();
    std::fs::write(
        config_home.join("st3/config.toml"),
        "person = \"person/config-operator\"\n",
    )
    .unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let source = r#"version 2
agent "queue-seat" { workspace "/tmp"; command "true" }
agent "queue-chief" {
  workspace "/tmp"
  command "true"
  queue-authority { move "client-v0-cli.queue-seat" }
}
mission "queued-work" state="ready" {
  concurrent-runs
  goal "Give the durable seat one step in each run."
  step "work" { assigned-to "agent/queue-seat" }
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
        .apply(&intent, &planned.subject_tokens, "cli-queue-missions")
        .unwrap();
    let mut runs = Vec::new();
    for index in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(2));
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "queued-work".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: format!("cli-queue-run-{index}"),
            })
            .unwrap();
        store
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap();
        runs.push(run);
    }
    let seat = runs[0].steps[0].assigned_to.clone().unwrap();
    let run = |index: usize| runs[index].subject.as_str();
    let step = |index: usize| runs[index].steps[0].subject.as_str();

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

    let queue =
        value(&run_queue_cli(&socket, &config_home, true, &["agents", "queue", &seat]).await);
    assert_eq!(queue["api_version"], "st3.client.v0");
    assert_eq!(queue["value"]["kind"], "agent-queue");
    assert_eq!(queue["value"]["next_work_id"], step(0));

    let bare = seat.strip_prefix("agent/").unwrap();
    let human = run_queue_cli(&socket, &config_home, false, &["agents", "queue", bare]).await;
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(
        human.starts_with(&format!("AGENT QUEUE  {seat}\n")),
        "{human}"
    );
    assert!(human.contains("CURRENT      none\n"), "{human}");
    assert!(
        human.contains(&format!("NEXT WORK    {}\n", step(0))),
        "{human}"
    );
    assert!(
        human.contains(&format!("  1. {}  ready  next {}\n", run(0), step(0))),
        "{human}"
    );
    assert!(
        human.contains(&format!("  3. {}  ready  {}\n", run(2), step(2))),
        "{human}"
    );
    assert!(human.contains("MOVES        0 total\n"), "{human}");

    let moved = run_queue_cli(
        &socket,
        &config_home,
        false,
        &[
            "agents",
            "queue",
            "move",
            &seat,
            run(2),
            "--top",
            "--reason",
            "the release needs it first",
            "--as",
            "person/queue-operator",
        ],
    )
    .await;
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let moved = String::from_utf8(moved.stdout).unwrap();
    assert!(
        moved.contains(&format!("  1. {}  ready  next {}\n", run(2), step(2))),
        "{moved}"
    );
    assert!(moved.contains("MOVES        1 total\n"), "{moved}");
    assert!(
        moved.contains(&format!(
            "  person/queue-operator moved {} to the top: the release needs it first\n",
            run(2)
        )),
        "{moved}"
    );

    let result = value(
        &run_queue_cli(
            &socket,
            &config_home,
            true,
            &["agents", "queue", "move", &seat, run(0), "--after", run(1)],
        )
        .await,
    );
    assert_eq!(result["value"]["status"], "completed");
    let queue =
        value(&run_queue_cli(&socket, &config_home, true, &["agents", "queue", &seat]).await);
    let order = queue["value"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["mission_run_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(order, [run(2), run(1), run(0)]);
    assert_eq!(
        queue["value"]["moves"][0]["actor_id"],
        "person/config-operator"
    );
    assert_eq!(queue["value"]["moves"][0]["placement"], "after");
    assert_eq!(queue["value"]["moves"][0]["anchor_run_id"], run(1));

    // Another agent moves the seat's runs as itself.
    let chief = "agent/client-v0-cli.queue-chief";
    let moved = run_queue_cli(
        &socket,
        &config_home,
        false,
        &[
            "agents",
            "queue",
            "move",
            &seat,
            run(1),
            "--top",
            "--reason",
            "the chief needs it first",
            "--as",
            chief,
        ],
    )
    .await;
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let moved = String::from_utf8(moved.stdout).unwrap();
    assert!(
        moved.contains(&format!("  1. {}  ready  next {}\n", run(1), step(1))),
        "{moved}"
    );
    assert!(
        moved.contains(&format!(
            "  {chief} moved {} to the top: the chief needs it first\n",
            run(1)
        )),
        "{moved}"
    );
    let claim = value(
        &run_queue_cli(
            &socket,
            &config_home,
            true,
            &[
                "agents",
                "queue",
                "move",
                &seat,
                run(0),
                "--before",
                run(1),
                "--as",
                chief,
            ],
        )
        .await,
    );
    assert_eq!(claim["kind"], "agent.queue.moved");
    assert_eq!(claim["actor"], chief);
    assert_eq!(claim["subject"], seat);
    assert_eq!(claim["body"]["fields"]["anchor"], run(1));
    let queue =
        value(&run_queue_cli(&socket, &config_home, true, &["agents", "queue", &seat]).await);
    assert_eq!(queue["value"]["runs"][0]["mission_run_id"], run(0));
    assert_eq!(queue["value"]["moves"][0]["actor_id"], chief);
    assert_eq!(queue["value"]["move_count"], 4);

    // Free mode: the seat moves its own queue without a grant.
    let claim = value(
        &run_queue_cli(
            &socket,
            &config_home,
            true,
            &[
                "agents",
                "queue",
                "move",
                &seat,
                run(2),
                "--top",
                "--as",
                &seat,
            ],
        )
        .await,
    );
    assert_eq!(claim["actor"], seat);
    let refused = run_queue_cli(
        &socket,
        &config_home,
        false,
        &[
            "agents",
            "queue",
            "move",
            &seat,
            run(2),
            "--top",
            "--as",
            "operator",
        ],
    )
    .await;
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("`person/NAME` or `agent/PATH`"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let refused = run_queue_cli(
        &socket,
        &config_home,
        false,
        &[
            "agents",
            "queue",
            "move",
            &seat,
            "mission-run/absent",
            "--bottom",
        ],
    )
    .await;
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("is not queued"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_and_missions_show_print_what_the_worker_last_reported() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let store = state.store.clone();
    let source = r#"version 2
agent "reporter" { workspace "/tmp"; command "true" }
mission "reported-work" state="ready" {
  goal "Show what the worker said."
  step "docs" { assigned-to "agent/reporter" }
  step "build" {
    title "Build the parser"
    assigned-to "agent/reporter"
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
        .apply(&intent, &planned.subject_tokens, "cli-reported-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "reported-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cli-reported-run".into(),
        })
        .unwrap();
    let step = |path: &str| {
        run.steps
            .iter()
            .find(|step| step.step == path)
            .unwrap()
            .clone()
    };
    let (docs, build) = (step("docs"), step("build"));
    let seat = build.assigned_to.clone().unwrap();
    let request = |key: &str, summary: &str| st3::model::WorkRequest {
        actor: Some(seat.clone()),
        incarnation: Some("reporter:1".into()),
        summary: Some(summary.into()),
        reason: None,
        evidence: Vec::new(),
        idempotency_key: key.into(),
    };
    for (subject, action, key, summary) in [
        (&docs.subject, "claim", "docs-claim", "Starting the docs"),
        (
            &docs.subject,
            "progress",
            "docs-progress",
            "Drafting the guide",
        ),
        (
            &docs.subject,
            "complete",
            "docs-complete",
            "Published the guide",
        ),
        (&build.subject, "claim", "build-claim", "Starting the build"),
        (
            &build.subject,
            "progress",
            "build-progress",
            "Tests pass; opening the pull request",
        ),
    ] {
        if action == "claim" {
            store.set_step_state(subject, "ready", None).unwrap();
        }
        store
            .work_action(subject, action, &request(key, summary))
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
    assert!(socket.exists(), "client-v0 test socket did not appear");

    let agent = run_cli_human(&socket, &["agents", "show", &seat]).await;
    assert!(
        agent.status.success(),
        "{}",
        String::from_utf8_lossy(&agent.stderr)
    );
    let agent = String::from_utf8(agent.stdout).unwrap();
    assert!(
        agent.contains(&format!(
            "CURRENT WORK {}\nCURRENT STEP Build the parser · working\n",
            build.subject
        )),
        "{agent}"
    );
    assert!(
        agent.contains("PROGRESS     Tests pass; opening the pull request · "),
        "{agent}"
    );
    assert!(
        agent.contains(&format!(
            "CURRENT WORK {}\nCURRENT STEP docs · verifying\nDONE         Published the guide\n",
            docs.subject
        )),
        "a submitted step awaiting verification shows its completion summary: {agent}"
    );

    let mission = run_cli_human(&socket, &["missions", "show", &run.subject]).await;
    assert!(
        mission.status.success(),
        "{}",
        String::from_utf8_lossy(&mission.stderr)
    );
    let mission = String::from_utf8(mission.stdout).unwrap();
    assert!(
        mission.contains("  progress: Tests pass; opening the pull request\n"),
        "{mission}"
    );
    assert!(
        mission.contains("  done: Published the guide\n"),
        "{mission}"
    );
    assert!(!mission.contains("Drafting the guide"), "{mission}");

    let json = value(&run_cli(&socket, &["missions", "show", &run.subject]).await);
    let steps = json["steps"].as_array().unwrap();
    let reported = |path: &str| steps.iter().find(|step| step["step"] == path).unwrap();
    assert_eq!(
        reported("build")["progress_summary"],
        "Tests pass; opening the pull request"
    );
    assert!(reported("build")["progress_at_unix_ms"].is_u64());
    assert!(reported("build").get("completion_summary").is_none());
    assert_eq!(
        reported("docs")["completion_summary"],
        "Published the guide"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missions_queued_cli_matches_agents_queue_show() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let config_home = root.path().join("config");
    std::fs::create_dir_all(config_home.join("st3")).unwrap();
    std::fs::write(
        config_home.join("st3/config.toml"),
        "person = \"person/config-operator\"\n",
    )
    .unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let source = r#"version 2
agent "queued-alias-seat" { workspace "/tmp"; command "true" }
mission "queued-alias-work" state="ready" {
  concurrent-runs
  goal "Give the durable seat one step."
  step "work" { assigned-to "agent/queued-alias-seat" }
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
        .apply(&intent, &planned.subject_tokens, "cli-queued-alias")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "queued-alias-work".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cli-queued-alias-run-0".into(),
        })
        .unwrap();
    store
        .set_step_state(&run.steps[0].subject, "ready", None)
        .unwrap();
    let seat = run.steps[0].assigned_to.clone().unwrap();

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

    let via_agents =
        value(&run_queue_cli(&socket, &config_home, true, &["agents", "queue", &seat]).await);
    let via_missions =
        value(&run_queue_cli(&socket, &config_home, true, &["missions", "queued", &seat]).await);
    // `request_id` is minted fresh per call; every other field must match exactly.
    assert_eq!(via_agents["value"], via_missions["value"]);
    assert_eq!(via_agents["snapshot"], via_missions["snapshot"]);
    assert_eq!(via_missions["value"]["kind"], "agent-queue");
    assert_eq!(via_missions["value"]["next_work_id"], run.steps[0].subject);

    let human_agents =
        run_queue_cli(&socket, &config_home, false, &["agents", "queue", &seat]).await;
    let human_missions =
        run_queue_cli(&socket, &config_home, false, &["missions", "queued", &seat]).await;
    assert!(human_agents.status.success());
    assert!(human_missions.status.success());
    assert_eq!(human_agents.stdout, human_missions.stdout);

    server.abort();
}

/// A runtime that starts nothing: a reconcile pass here only writes a run's own declarations.
struct NoRuntime;

impl st3::reconcile::RuntimeControl for NoRuntime {
    fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
        Ok(Vec::new())
    }
    fn observe_exec(&self, _: &str) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
        Ok(None)
    }
    fn start(&self, _: &st3::model::MemberSpec) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
        Ok(())
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

/// Run one reconcile pass as the store's own node, which writes each of its running missions'
/// lane declarations.
fn materialize_run_declarations(store: &Arc<Store>) {
    st3::reconcile::Reconciler::new(
        store.clone(),
        Arc::new(NoRuntime),
        "client-v0-cli".into(),
        Arc::new(Notify::new()),
    )
    .reconcile_once()
    .unwrap();
}

async fn run_lane_cli(
    socket: &Path,
    config_home: &Path,
    agent: Option<&str>,
    json: bool,
    args: &[&str],
) -> Output {
    let binary = assert_cmd::cargo::cargo_bin!("st3-fixture").to_path_buf();
    let socket = socket.to_path_buf();
    let config_home = config_home.to_path_buf();
    let agent = agent.map(str::to_owned);
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        let mut command = st3::test_support::command(binary);
        command
            .env_remove("ST_AGENT")
            .env_remove("ST_MISSION_RUN")
            .env("XDG_CONFIG_HOME", config_home)
            .arg("--endpoint")
            .arg(socket);
        if let Some(agent) = agent {
            command.env("ST_AGENT", agent);
        }
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    })
    .await
    .unwrap()
}

fn lane_order(lane: &Value) -> Vec<String> {
    lane["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry["entry"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn failure(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "expected a refusal, got {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lanes_cli_keeps_one_ordered_lane_that_people_and_agents_change() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let config_home = root.path().join("config");
    std::fs::create_dir_all(config_home.join("st3")).unwrap();
    std::fs::write(
        config_home.join("st3/config.toml"),
        "person = \"person/config-operator\"\n",
    )
    .unwrap();
    let state = test_state(root.path());
    let store = state.store.clone();
    let source = r#"version 2
agent "lane-driver" { workspace "/tmp"; command "true" }
mission "example/merge-train" state="ready" {
  goal "Merge ready changes into main one at a time."
  lane "app" {
    entries "resource/github/acme/app/ci/pull-request/"
    approver "person/ada"
  }
  step "drive" { assigned-to "agent/lane-driver" }
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
        .apply(&intent, &planned.subject_tokens, "cli-lane-mission")
        .unwrap();
    let run = store
        .create_mission_run(&MissionRunRequest {
            mission: "example/merge-train".into(),
            revision: None,
            workspace: "/tmp".into(),
            requester: Some("person/requester".into()),
            mode: Some("run".into()),
            inputs: BTreeMap::new(),
            idempotency_key: "cli-lane-run".into(),
        })
        .unwrap();
    materialize_run_declarations(&store);
    let lane = format!(
        "lane/{}/app",
        run.subject.strip_prefix("mission-run/").unwrap()
    );
    let prefix = "resource/github/acme/app/ci/pull-request/";

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
    let cli = |agent: Option<&'static str>, json: bool, args: Vec<String>| {
        let socket = socket.clone();
        let config_home = config_home.clone();
        async move {
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            run_lane_cli(&socket, &config_home, agent, json, &args).await
        }
    };
    let args = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();

    // The run's declaration made one open lane that `ls` lists and a short name finds.
    let listed = cli(None, false, args(&["lanes", "ls"])).await;
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.starts_with("LANES  1\n"), "{listed}");
    assert!(
        listed.contains(&format!("  {lane}  0 entries\n")),
        "{listed}"
    );

    // A person joins with the configured identity, an agent names itself, and a repeated join
    // records nothing.
    let joined = value(
        &cli(
            None,
            true,
            args(&["lanes", "join", "app", "42", "--reason", "green"]),
        )
        .await,
    );
    assert!(joined["claim"]["id"].is_string(), "{joined}");
    assert_eq!(joined["lane"]["subject"], lane);
    assert_eq!(joined["lane"]["entries"][0]["entry"], format!("{prefix}42"));
    assert_eq!(
        joined["lane"]["entries"][0]["joined_by"],
        "person/config-operator"
    );
    assert_eq!(joined["lane"]["entries"][0]["state"], "waiting");
    let joined = value(
        &cli(
            None,
            true,
            args(&["lanes", "join", &lane, "#43", "--as", "agent/lane-worker"]),
        )
        .await,
    );
    assert_eq!(lane_order(&joined["lane"]), ["42", "43"]);
    let again = value(&cli(None, true, args(&["lanes", "join", "app", "42"])).await);
    assert!(again["claim"].is_null(), "{again}");
    assert_eq!(lane_order(&again["lane"]), ["42", "43"]);

    // A harness joins as its own seat without naming it, and cannot borrow a person.
    let harness = value(
        &cli(
            Some("agent/lane-harness"),
            true,
            args(&["lanes", "join", "app", "44"]),
        )
        .await,
    );
    assert_eq!(
        harness["lane"]["entries"][2]["joined_by"],
        "agent/lane-harness"
    );
    let borrowed = failure(
        &cli(
            Some("agent/lane-harness"),
            true,
            args(&["lanes", "approve", "app", "43", "--as", "person/ada"]),
        )
        .await,
    );
    assert!(
        borrowed.contains("cannot act as `person/ada`"),
        "{borrowed}"
    );

    // Moves in each placement; a move that names an entry outside the lane is refused.
    let moved = value(&cli(None, true, args(&["lanes", "move", "app", "44", "--top"])).await);
    assert_eq!(lane_order(&moved["lane"]), ["44", "42", "43"]);
    let moved = value(
        &cli(
            None,
            true,
            args(&["lanes", "move", "app", "43", "--before", "44"]),
        )
        .await,
    );
    assert_eq!(lane_order(&moved["lane"]), ["43", "44", "42"]);
    let moved = value(
        &cli(
            None,
            true,
            args(&[
                "lanes",
                "move",
                "app",
                "43",
                "--bottom",
                "--reason",
                "main moved",
            ]),
        )
        .await,
    );
    assert_eq!(lane_order(&moved["lane"]), ["44", "42", "43"]);
    let missing = failure(
        &cli(
            None,
            true,
            args(&["lanes", "move", "app", "42", "--after", "99"]),
        )
        .await,
    );
    assert!(
        missing.contains(&format!("`{prefix}99` is not in")),
        "{missing}"
    );
    let outside = failure(
        &cli(
            None,
            true,
            args(&["lanes", "join", "app", "resource/other/1"]),
        )
        .await,
    );
    assert!(
        outside.contains(&format!("start with `{prefix}`")),
        "{outside}"
    );

    // The run records a status on the exact head it applies to.
    let marked = value(
        &cli(
            None,
            true,
            args(&[
                "lanes",
                "mark",
                "app",
                "42",
                "--state",
                "running",
                "--detail",
                "testing 1f2e3d4",
                "--head",
                "1f2e3d4",
                "--as",
                "agent/lane-driver",
            ]),
        )
        .await,
    );
    let entry = &marked["lane"]["entries"][1];
    assert_eq!(entry["state"], "running");
    assert_eq!(entry["detail"], "testing 1f2e3d4");
    assert_eq!(entry["head"], "1f2e3d4");
    assert_eq!(entry["marked_by"], "agent/lane-driver");

    // Only the declared approver approves.
    let denied = failure(&cli(None, true, args(&["lanes", "approve", "app", "43"])).await);
    assert!(denied.contains("only person/ada approves"), "{denied}");
    let approved = value(
        &cli(
            None,
            true,
            args(&["lanes", "approve", "app", "43", "--as", "person/ada"]),
        )
        .await,
    );
    assert_eq!(approved["lane"]["entries"][2]["approved_by"], "person/ada");

    // An entry leaves with an outcome; a rejoin goes to the back without its old approval.
    let left = value(
        &cli(
            None,
            true,
            args(&[
                "lanes",
                "leave",
                "app",
                "43",
                "--outcome",
                "completed",
                "--reason",
                "merged",
            ]),
        )
        .await,
    );
    assert_eq!(lane_order(&left["lane"]), ["44", "42"]);
    let rejoined = value(&cli(None, true, args(&["lanes", "join", "app", "43"])).await);
    assert_eq!(lane_order(&rejoined["lane"]), ["44", "42", "43"]);
    assert!(rejoined["lane"]["entries"][2]["approved_by"].is_null());

    // The human view lists entries in order and history newest first.
    let shown = cli(None, false, args(&["lanes", "show", &run.subject])).await;
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown = String::from_utf8(shown.stdout).unwrap();
    assert!(
        shown.starts_with(&format!("LANE      {lane}\nRUN       {}\n", run.subject)),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "ENTRIES   {prefix}\nAPPROVER  person/ada\nQUEUE     3\n"
        )),
        "{shown}"
    );
    assert!(
        shown.contains("  1. 44  waiting  joined by agent/lane-harness "),
        "{shown}"
    );
    assert!(
        shown.contains("  2. 42  running  testing 1f2e3d4  joined by person/config-operator "),
        "{shown}"
    );
    assert!(
        shown.contains("  3. 43  waiting  joined by person/config-operator "),
        "{shown}"
    );
    let recent = shown.split("RECENT\n").nth(1).unwrap();
    assert!(
        recent.starts_with("  joined 43 by person/config-operator "),
        "{shown}"
    );
    assert!(
        recent.contains("  43 left (completed) by person/config-operator "),
        "{shown}"
    );
    assert!(recent.contains(": merged\n"), "{shown}");
    assert!(
        recent.contains("  moved 43 to the bottom by person/config-operator "),
        "{shown}"
    );
    assert!(recent.contains("  approved 43 by person/ada "), "{shown}");

    // Mission views show the lane the run owns.
    let mission = cli(None, false, args(&["missions", "show", &run.subject])).await;
    let mission = String::from_utf8(mission.stdout).unwrap();
    assert!(
        mission.contains(&format!("\nLANES\n  {lane}  3 entries\n    1. 44  waiting")),
        "{mission}"
    );
    let tree = value(&cli(None, true, args(&["missions", "tree"])).await);
    assert_eq!(tree["value"]["lanes"][0]["id"], lane, "{tree}");
    assert_eq!(
        tree["value"]["lanes"][0]["entries"][1]["label"], "42",
        "{tree}"
    );
    let tree = cli(None, false, args(&["missions", "tree"])).await;
    let tree = String::from_utf8(tree.stdout).unwrap();
    assert!(
        tree.contains(&format!(
            "LANES\n  {lane}  3 entries · front 44 waiting\nUNSTARTED MISSIONS\n"
        )),
        "{tree}"
    );

    // A lane closes with its run: it leaves the list and refuses changes.
    store
        .set_mission_run_state(&run.subject, "completed", "terminal", Some("done"))
        .unwrap();
    let open = value(&cli(None, true, args(&["lanes", "ls"])).await);
    assert_eq!(open.as_array().unwrap().len(), 0, "{open}");
    let all = value(&cli(None, true, args(&["lanes", "ls", "--all"])).await);
    assert_eq!(all[0]["open"], false, "{all}");
    let closed = failure(&cli(None, true, args(&["lanes", "join", &lane, "45"])).await);
    assert!(closed.contains("is closed"), "{closed}");

    server.abort();
}

#[test]
fn help_starts_with_examples_and_keeps_plumbing_reachable() {
    let help = |args: &[&str]| {
        let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let default = help(&["--help"]);
    assert_eq!(default, help(&["help"]));
    assert!(default.starts_with("See what needs you:\n  st now\n"));
    assert!(default.contains("st agents new NAME --harness claude --attach"));
    assert!(default.contains("Everyday use:"));
    assert!(default.contains("Inside an agent seat:"));
    assert!(default.contains("Running st on a machine or fleet:"));
    assert!(!default.contains("Plumbing:"));
    assert!(help(&["help", "--all"]).contains("Plumbing:"));
    assert_eq!(
        help(&["help", "--all"]),
        help(&["--endpoint", "help", "help", "--all"])
    );
    assert!(!help(&["agents", "ls", "--all", "--help"]).contains("Plumbing:"));
    for path in [
        vec!["schema"],
        vec!["agents", "new"],
        vec!["missions", "start"],
    ] {
        let mut flag = path.clone();
        flag.push("--help");
        let mut named = vec!["help"];
        named.extend(path.clone());
        assert_eq!(help(&flag), help(&named));
        let mut nested = path;
        nested.insert(1, "help");
        assert_eq!(help(&flag), help(&nested));
    }
    let output = st3::test_support::command(assert_cmd::cargo::cargo_bin!("st3-fixture"))
        .args(["help", "missing-command"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

// Serve the real API without a reconciler: these tests create graph declarations only.
async fn serve_creation_api(
    root: &Path,
    harness_state: Option<&'static str>,
) -> (PathBuf, tokio::task::JoinHandle<Result<(), anyhow::Error>>) {
    let socket = root.join("st3.sock");
    let state = test_state(root);
    let store = state.store.clone();
    let router = st3::api::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let store = store.clone();
            async move {
                let applied = matches!(
                    request.uri().path(),
                    "/v1/intent/apply" | "/v1/client/actions"
                );
                let response = next.run(request).await;
                // Simulate the observations a newly started process would publish.
                if applied
                    && response.status().is_success()
                    && let Some(harness_state) = harness_state
                {
                    for (kind, fields) in [
                        (
                            "runtime.observed",
                            serde_json::json!({
                                "runtime_id": "demo", "incarnation_id": "demo:1",
                                "status": "running", "reachability": "reachable"
                            }),
                        ),
                        (
                            "harness.observed",
                            serde_json::json!({
                                "driver": "omp", "incarnation_id": "demo:1", "state": harness_state
                            }),
                        ),
                    ] {
                        store
                            .append_claim(&ClaimInput {
                                subject: "agent/client-v0-cli.demo".into(),
                                kind: kind.into(),
                                actor: Some("agent/client-v0-cli.demo".into()),
                                fields: serde_json::from_value(fields).unwrap(),
                                evidence: Vec::new(),
                                expected_subject: None,
                                idempotency_key: None,
                            })
                            .unwrap();
                    }
                }
                response
            }
        },
    ));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, router).await });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(socket.exists());
    (socket, server)
}

async fn agent_declaration(socket: &Path, subject: &str) -> st3::model::DesiredSubject {
    let client = st3::client::Client::unix(socket);
    let status: st3::model::StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(subject)
        ))
        .await
        .unwrap();
    let token = status.subjects[0].desired_token.as_ref().unwrap();
    let claim: st3::model::ClaimRecord = client
        .get(&format!("/v1/claims/by-id/{token}"))
        .await
        .unwrap();
    serde_json::from_value(claim.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_start_preserves_command_declarations_and_refuses_harness_options() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve_creation_api(root.path(), None).await;
    for (name, launch) in [
        ("command", r#"command "omp --model original""#),
        ("argv", r#"argv "omp" "--model" "original""#),
    ] {
        let source = root.path().join(format!("{name}.kdl"));
        std::fs::write(
            &source,
            format!(
                "version 2\nhost \"remote\" {{\n agent \"{name}\" {{\n\
                 workspace \"/original\"\n restart never\n {launch}\n\
                 env {{ ORIGINAL \"kept\" }}\n }}\n}}\n"
            ),
        )
        .unwrap();
        value(
            &run_cli(
                &socket,
                &[
                    "agents",
                    "apply",
                    source.to_str().unwrap(),
                    "--as",
                    "person/avery",
                ],
            )
            .await,
        );
        let subject = format!("agent/remote.{name}");
        let original = agent_declaration(&socket, &subject).await;
        for stopped in [false, true] {
            if stopped {
                value(
                    &run_cli(
                        &socket,
                        &["agents", "stop", &subject, "--as", "person/avery"],
                    )
                    .await,
                );
            }
            let preview = run_cli(
                &socket,
                &[
                    "agents",
                    "start",
                    &subject,
                    "--as",
                    "person/avery",
                    "--print-kdl",
                ],
            )
            .await;
            assert!(preview.status.success(), "{preview:?}");
            let intent =
                st3::parse_intent(&String::from_utf8(preview.stdout).unwrap(), "client-v0-cli")
                    .unwrap();
            assert_eq!(intent.subjects[&subject].member, original.member);
            value(
                &run_cli(
                    &socket,
                    &["agents", "start", &subject, "--as", "person/avery"],
                )
                .await,
            );
            assert_eq!(
                agent_declaration(&socket, &subject).await.member,
                original.member
            );
        }
        for option in ["--model", "--effort", "--arg"] {
            let rejected = run_cli(
                &socket,
                &[
                    "agents",
                    "start",
                    &subject,
                    "--as",
                    "person/avery",
                    option,
                    "new",
                ],
            )
            .await;
            assert!(!rejected.status.success(), "{rejected:?}");
            let error = String::from_utf8(rejected.stderr).unwrap();
            assert!(
                error.contains(name) && error.contains("typed harness"),
                "{error}"
            );
            assert_eq!(
                agent_declaration(&socket, &subject).await.member,
                original.member
            );
        }
        value(
            &run_cli(
                &socket,
                &[
                    "agents",
                    "start",
                    &subject,
                    "--as",
                    "person/avery",
                    "--host",
                    "moved",
                    "--workspace",
                    root.path().to_str().unwrap(),
                ],
            )
            .await,
        );
        let moved = agent_declaration(&socket, &subject).await;
        let mut expected = original.member.unwrap();
        expected.host = "moved".into();
        expected.workspace = root.path().to_str().unwrap().into();
        expected.cwd = expected.workspace.clone();
        assert_eq!(moved.member, Some(expected));
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_start_patches_only_explicit_typed_harness_fields() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve_creation_api(root.path(), None).await;
    let source = root.path().join("harness.kdl");
    std::fs::write(
        &source,
        "version 2\nagent \"example/typed\" {\n host \"remote\"\n\
         workspace \"/original\"\n restart never\n env { ORIGINAL \"kept\" }\n\
         harness omp { model \"old\"; effort \"low\"; args \"--old\" }\n}\n",
    )
    .unwrap();
    value(
        &run_cli(
            &socket,
            &[
                "agents",
                "apply",
                source.to_str().unwrap(),
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    let subject = "agent/example/typed";
    let original = agent_declaration(&socket, subject).await;
    for (option, model, effort, arguments, driver) in [
        (vec![], "old", "low", vec!["--old"], "omp"),
        (vec!["--model", "new"], "new", "low", vec!["--old"], "omp"),
        (
            vec!["--effort", "high"],
            "new",
            "high",
            vec!["--old"],
            "omp",
        ),
        (
            vec!["--arg=--new", "--arg", "value"],
            "new",
            "high",
            vec!["--new", "value"],
            "omp",
        ),
        (
            vec!["--harness", "pi"],
            "new",
            "high",
            vec!["--new", "value"],
            "pi",
        ),
    ] {
        let mut args = vec!["agents", "start", subject, "--as", "person/avery"];
        args.extend(option.iter().copied());
        value(&run_cli(&socket, &args).await);
        let current = agent_declaration(&socket, subject).await;
        let member = current.member.unwrap();
        let previous = original.member.as_ref().unwrap();
        assert_eq!(member.host, previous.host);
        assert_eq!(member.workspace, previous.workspace);
        assert_eq!(member.restart, previous.restart);
        assert_eq!(member.environment, previous.environment);
        let nodes = current.desired["children"].as_array().unwrap();
        let harness = nodes.iter().find(|node| node["name"] == "harness").unwrap();
        let children = harness["children"].as_array().unwrap();
        let field =
            |name: &str| &children.iter().find(|node| node["name"] == name).unwrap()["arguments"];
        assert_eq!(field("model"), &serde_json::json!([model]));
        assert_eq!(field("effort"), &serde_json::json!([effort]));
        assert_eq!(field("args"), &serde_json::json!(arguments));
        assert_eq!(harness["arguments"][0], driver);
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_start_accepts_explicit_identity_and_rejects_doubled_prefixes() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve_creation_api(root.path(), None).await;
    for (identity, subject) in [
        ("example/worker", "agent/example/worker"),
        ("agent/example/worker", "agent/example/worker"),
        ("worker", "agent/placement.worker"),
        ("agent/other.worker", "agent/other.worker"),
    ] {
        let output = run_cli(
            &socket,
            &[
                "agents",
                "start",
                identity,
                "--host",
                "placement",
                "--as",
                "person/avery",
            ],
        )
        .await;
        let applied = value(&output);
        assert!(
            applied["subject_tokens"].get(subject).is_some(),
            "{identity}: {applied}"
        );
        let preview = run_cli(
            &socket,
            &[
                "agents",
                "start",
                identity,
                "--host",
                "placement",
                "--as",
                "person/avery",
                "--print-kdl",
            ],
        )
        .await;
        assert!(preview.status.success());
        let kdl = String::from_utf8(preview.stdout).unwrap();
        let intent = st3::parse_intent(&kdl, "client-v0-cli").unwrap();
        assert!(intent.subjects.contains_key(subject), "{kdl}");
    }
    for preview in [false, true] {
        let mut args = vec![
            "agents",
            "start",
            "agent/agent/example/accidental",
            "--as",
            "person/avery",
        ];
        if preview {
            args.push("--print-kdl");
        }
        let output = run_cli(&socket, &args).await;
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("agent/agent/"), "{error}");
        assert!(error.contains("ID or agent/ID"), "{error}");
    }
    let agents = value(&run_cli(&socket, &["agents", "ls", "--all"]).await);
    assert!(
        !agents.to_string().contains("accidental"),
        "rejected identity created a seat: {agents}"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_agent_explains_ready_starting_and_waiting_states_and_preserves_json() {
    for (harness, json) in [
        (None, false),
        (Some("ready"), false),
        (Some("blocked"), false),
        (Some("ready"), true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (socket, server) = serve_creation_api(root.path(), harness).await;
        let workspace = root.path().to_str().unwrap();
        let output = run_cli_mode(
            &socket,
            json,
            &[
                "agents",
                "new",
                "demo",
                "--harness",
                "omp",
                "--model",
                "example-model",
                "--workspace",
                workspace,
                "--as",
                "person/avery",
                "--timeout",
                "100ms",
            ],
        )
        .await;
        if json {
            let response = value(&output);
            assert_eq!(
                response,
                serde_json::json!({
                    "subject": "agent/client-v0-cli.demo", "host_id": "host/client-v0-cli",
                    "workspace": workspace, "state": "running", "harness_state": "ready",
                })
            );
        } else {
            let rendered = String::from_utf8(output.stdout).unwrap();
            let progress = String::from_utf8(output.stderr).unwrap();
            assert!(!progress.contains("unobserved"), "{progress}");
            let expected = match harness {
                None => "Still starting — waiting for the agent process to appear.",
                Some("ready") => "Ready — the agent is running.",
                _ => "Waiting for you",
            };
            assert!(rendered.contains(expected), "{rendered}\n{progress}");
            assert_eq!(
                output.status.success(),
                harness == Some("ready"),
                "{progress}"
            );
            for action in [
                "st terminals attach agent/client-v0-cli.demo --as person/avery",
                "st conversations send agent/client-v0-cli.demo --from person/avery --body 'Hello'",
                "st agents show agent/client-v0-cli.demo",
                "st agents stop agent/client-v0-cli.demo --as person/avery",
                "--attach",
            ] {
                assert!(rendered.contains(action), "{rendered}");
            }
        }
        server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_commands_end_with_actions_and_json_remains_parseable() {
    let root = tempfile::tempdir().unwrap();
    let (socket, server) = serve_creation_api(root.path(), None).await;
    for json in [false, true] {
        let output = run_cli_mode(
            &socket,
            json,
            &["devices", "pair", "demo-phone", "--as", "person/avery"],
        )
        .await;
        if json {
            let response = value(&output);
            assert_eq!(response["value"]["kind"], "pairing-challenge");
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = String::from_utf8(output.stdout).unwrap();
            assert!(rendered.contains("Waiting for your device"), "{rendered}");
            assert!(
                rendered.contains("st devices --as person/avery"),
                "{rendered}"
            );
        }
    }
    let request = root.path().join("request.txt");
    std::fs::write(&request, "Build an example site.").unwrap();
    let mission = root.path().join("mission.kdl");
    std::fs::write(&mission, "version 2\nmission \"demo\" state=\"ready\" {\n concurrent-runs max=2\n goal \"Build an example.\"\n step \"work\" {}\n}\n").unwrap();
    value(
        &run_cli(
            &socket,
            &[
                "missions",
                "publish",
                mission.to_str().unwrap(),
                "--as",
                "person/avery",
            ],
        )
        .await,
    );
    for json in [false, true] {
        let id = if json { "demo/json" } else { "demo/human" };
        let output = run_cli_mode(
            &socket,
            json,
            &[
                "missions",
                "start",
                "demo",
                "--id",
                id,
                "--as",
                "person/avery",
                "--workspace",
                root.path().to_str().unwrap(),
            ],
        )
        .await;
        if json {
            let response = value(&output);
            assert_eq!(
                response["mission_run"]["subject"],
                format!("mission-run/{id}")
            );
            assert!(response["publication"].is_object());
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = String::from_utf8(output.stdout).unwrap();
            assert!(
                rendered.contains("st missions show mission-run/demo/human --follow"),
                "{rendered}"
            );
            assert!(
                rendered.contains(
                    "st missions cancel mission-run/demo/human --as person/avery --reason"
                ),
                "{rendered}"
            );
        }
        let output = run_cli_mode(
            &socket,
            json,
            &[
                "launch",
                "start",
                "--id",
                id,
                request.to_str().unwrap(),
                "--as",
                "person/avery",
                "--workspace",
                root.path().to_str().unwrap(),
            ],
        )
        .await;
        if json {
            let response = value(&output);
            assert!(
                response["subject"]
                    .as_str()
                    .unwrap()
                    .starts_with("planning-session/launch/demo/json/")
            );
            assert_eq!(response["status"], "planning");
        } else {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rendered = String::from_utf8(output.stdout).unwrap();
            assert!(rendered.contains("Started — the planner"), "{rendered}");
            assert!(
                rendered.contains("st launch show planning-session/launch/demo/human/"),
                "{rendered}"
            );
            assert!(
                rendered.contains("st launch cancel planning-session/launch/demo/human/"),
                "{rendered}"
            );
        }
    }
    server.abort();
}
