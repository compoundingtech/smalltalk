#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use st3::api::AppState;
use st3::model::{ClaimInput, IntentInput, PlannerSpec};
use st3::store::Store;
use tokio::sync::{Notify, watch};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authored_omp_resume_publishes_binding_from_an_empty_managed_directory() {
    let root = tempfile::tempdir().unwrap();
    let subject = "agent/garden/worker";
    let id = "5f9a6e16-5e30-4bce-b327-9a8241321bd6";
    let legacy = root.path().join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    let transcript = legacy.join(format!("2026-10-04_{id}.jsonl"));
    std::fs::write(
        &transcript,
        format!("{}\n{}\n",
            json!({"type":"session","id":id,"timestamp":"2026-09-25T14:00:00Z","cwd":root.path()}),
            json!({"type":"message","id":"answer-a","message":{"role":"assistant","content":[{"type":"text","text":"Seat A's resumed conversation"}]}})
        ),
    )
    .unwrap();
    let drivers = root.path().join("drivers");
    let managed = drivers
        .join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24])
        .join("sessions/omp/provider-sessions");
    std::fs::create_dir_all(&managed).unwrap();
    let provider = root.path().join("omp");
    // The fake provider uses the actual spawned channel, not a direct API binding request.
    std::fs::write(&provider, format!(r#"#!{bash}
if [ "$1" = "--version" ]; then echo '18.4.4'; exit 0; fi
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--resume" ]; then shift; resume="$1"; fi
  shift
done
{{
  printf '%s\n' '{{"type":"session","sessionId":"{id}","sessionFile":"'"$resume"'"}}'
  while [ -d "${{RESUME_TEST_STOP%/*}}" ] && [ ! -f "$RESUME_TEST_STOP" ]; do sleep 0.01; done
}} | "$ST_OMP_CHANNEL_BIN" --endpoint "$ST3_ENDPOINT" driver omp-channel --identity "$ST_OMP_CHANNEL_IDENTITY" --catalog "$ST_DRIVER_ROOT"
"#, bash=env!("ST3_FIXTURE_BASH"))).unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!(
        r#"version 2
agent "garden/worker" {{
 workspace "{}"
 harness "omp" {{ args "--resume" "{}"; }}
}}
"#,
        root.path().display(),
        transcript.display()
    );
    let store = Arc::new(Store::open(&root.path().join("graph.db"), "resume-link-test").unwrap());
    let intent = st3::parse_intent(&source, "resume-link-test").unwrap();
    let preview = store
        .mission(
            &intent,
            IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply_as(
            &intent,
            &preview.subject_tokens,
            "declare",
            Some("person/avery"),
        )
        .unwrap();
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("status".into(), json!("running")),
                ("incarnation_id".into(), json!("worker:current")),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let socket = root.path().join("api.sock");
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "resume-link-test".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: PlannerSpec::default(),
    };
    let listener_socket = socket.clone();
    let state_socket = root.path().join("state.sock");
    let server = tokio::spawn(async move {
        st3::api::serve_unix_bound(&listener_socket, &state_socket, st3::api::router(state)).await
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let hooks = root.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    std::fs::write(hooks.join(st_drivers::hooks::ST3_SET_MARKER), "fixture").unwrap();
    std::fs::write(hooks.join("omp-channel.ts"), "").unwrap();
    let mut command = st3::test_support::async_command(env!("CARGO_BIN_EXE_st3-fixture"));
    let stop = root.path().join("provider-stop");
    command
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", env!("ST3_FIXTURE_PATH"))
        .env("RESUME_TEST_STOP", &stop)
        .env("ST3_DRIVER_STATE_DIR", &drivers)
        .env("ST_AGENT", subject)
        .env("ST3_ENDPOINT", &socket)
        .env("ST3_MAILBOX_TRANSPORT", "push")
        .env("ST_HOOKS", hooks)
        .args(["--endpoint"])
        .arg(&socket)
        .args(["driver", "omp", "--subject", subject, "--"])
        .arg(provider)
        .arg("--resume")
        .arg(&transcript);
    let completion = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(30), command.kill_on_drop(true).output())
            .await
            .expect("native start completes")
            .unwrap()
    });
    for _ in 0..1000 {
        if store
            .latest_claim(subject, Some("harness.session-file"))
            .unwrap()
            .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let binding = store
        .latest_claim(subject, Some("harness.session-file"))
        .unwrap();
    let client = st3::client::Client::unix(socket.clone());
    let sessions: Value = client.get("/v1/client/sessions").await.unwrap();
    let session_id = sessions["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["owner_id"] == subject)
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let timeline_path = format!(
        "/v1/client/sessions/{}/timeline",
        urlencoding::encode(session_id)
    );
    let before: Value = client.get(&timeline_path).await.unwrap();
    assert!(
        before["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["body"]["text"] == "Seat A's resumed conversation")
    );
    // Exercise the actual timeline endpoint after another seat writes a newer sibling.
    let sibling_id = "9927ab58-9d80-4c97-b92d-0c17f688b331";
    std::fs::write(legacy.join(format!("2026-10-05_{sibling_id}.jsonl")), format!("{}\n{}\n",
        json!({"type":"session","id":sibling_id,"timestamp":"2026-10-05T16:00:00Z","cwd":root.path()}),
        json!({"type":"message","id":"answer-b","message":{"role":"assistant","content":[{"type":"text","text":"Seat B's unrelated conversation"}]}})
    )).unwrap();
    let after: Value = client.get(&timeline_path).await.unwrap();
    assert_eq!(after["items"], before["items"]);
    std::fs::write(&stop, "").unwrap();
    let output = completion.await.unwrap();
    server.abort();
    assert_eq!(std::fs::read_link(&managed).unwrap(), legacy);
    // This fixture has no PTY/reconciler. Releasing its provider can fence the delivery
    // subscription during teardown; the contract under test is the binding while it is live.
    let binding = binding.unwrap_or_else(|| panic!("native binding was not published: {output:?}"));
    assert_eq!(binding.body["fields"]["session_id"], id);
    assert_eq!(binding.body["fields"]["path"], transcript.to_str().unwrap());
    assert_eq!(binding.body["fields"]["incarnation_id"], "worker:current");
}
