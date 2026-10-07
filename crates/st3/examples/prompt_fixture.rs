//! Finite, isolated client qualification fixture. This is not a vendor emulator:
//! the actual native invocation socket and client action run against an invented seat.
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use st_drivers::prompts::{Answer, Prompt};
use st3::model::{ClaimInput, IntentInput};
use st3::store::Store;
use tokio::sync::{Notify, watch};

const SEAT: &str = "agent/garden/orchard";

fn runtime(store: &Store, incarnation: &str) -> Result<()> {
    store.append_claim(&ClaimInput {
        subject: SEAT.into(),
        kind: "runtime.observed".into(),
        actor: Some(SEAT.into()),
        fields: BTreeMap::from([
            ("status".into(), json!("running")),
            ("incarnation_id".into(), json!(incarnation)),
        ]),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    })?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let seconds: u64 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "90".into())
        .parse()?;
    ensure!(
        (10..=180).contains(&seconds),
        "fixture duration must be 10 through 180 seconds"
    );
    let directory = tempfile::Builder::new().prefix("st-prompt-ui-").tempdir()?;
    let root = directory.path();
    let native = root.join("native");
    std::fs::create_dir(&native)?;
    let store = Arc::new(Store::open(&root.join("graph.db"), "prompt-fixture")?);
    let intent = st3::parse_intent(
        "version 2\nagent \"garden/orchard\" { name \"Orchard\"; command \"true\" }",
        store.origin(),
    )?;
    let preview = store.mission(
        &intent,
        IntentInput {
            kdl: String::new(),
            source_name: None,
        },
    )?;
    store.apply_as(
        &intent,
        &preview.subject_tokens,
        "fixture-person",
        Some("person/ada"),
    )?;
    runtime(&store, "runtime-a")?;
    st_drivers::harness_events::enable(&native, "runtime-a")?;
    st_drivers::harness_state::claim(&native, "garden/orchard", "claude", "provider-a")?;
    let stopped = Arc::new(AtomicBool::new(false));
    let hook = std::thread::spawn({
        let native = native.clone();
        let cancel = root.join("cancel");
        let stopped = stopped.clone();
        move || -> Result<Vec<u8>> {
            let mut output = Vec::new();
            st_drivers::prompts::run_claude(
                &native,
                "provider-a",
                &json!({
                    "session_id":"fixture-session", "tool_name":"Bash",
                    "tool_input":{"command":"printf fixture-ui"}
                }),
                Duration::from_secs(seconds - 5),
                &mut output,
                &|| stopped.load(Ordering::SeqCst) || cancel.exists(),
            )?;
            Ok(output)
        }
    });
    let socket = root.join("api.sock");
    let state = st3::api::AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "prompt-fixture".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "false".into(),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: Some(root.into()),
        planner_default: Default::default(),
    };
    let server = tokio::spawn({
        let socket = socket.clone();
        let app = st3::api::router(state.clone());
        async move { st3::api::serve_unix(&socket, app).await }
    });
    let history = tokio::spawn(store.clone().run_attention_history());
    println!(
        "{}",
        json!({"fixture_root":root,"endpoint":socket,"person":"person/ada","seat":SEAT,"seconds":seconds})
    );
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut side_answered = false;
    let mut restarted = false;
    loop {
        // Only files inside this freshly created private fixture can control it.
        let mut changed = false;
        if root.join("restart").exists() && !restarted {
            runtime(&store, "runtime-b")?;
            changed = true;
            restarted = true;
            stopped.store(true, Ordering::SeqCst);
        }
        let answer_file = root.join("terminal-answer");
        if answer_file.exists() && !side_answered && !restarted {
            ensure!(
                std::fs::metadata(&answer_file)?.len() <= 16,
                "fixture side answer exceeds bounds"
            );
            let answer_id = std::fs::read_to_string(&answer_file)?.trim().to_owned();
            ensure!(
                matches!(answer_id.as_str(), "approve" | "deny"),
                "fixture side answer must be explicit approve or deny"
            );
            if let Some(prompt) =
                st_drivers::harness_events::live_prompts(&native, "runtime-a")?.first()
            {
                let answer = Answer {
                    episode: prompt.episode.clone(),
                    prompt_id: prompt.prompt_id.clone(),
                    runtime_incarnation: prompt.runtime_incarnation.clone(),
                    answer_id,
                };
                let result =
                    st_drivers::prompts::send_answer(prompt.endpoint.as_deref().unwrap(), &answer)?;
                ensure!(
                    result["accepted"] == true,
                    "native fixture refused side answer"
                );
                side_answered = true;
            }
        }
        for event in st_drivers::harness_events::pending(&native, 32)? {
            if event.kind == "harness-prompt" && !restarted {
                let mut payload = event.payload.clone();
                payload.as_object_mut().unwrap().remove("account_ref");
                let prompt: Prompt = serde_json::from_value(payload)?;
                store.append_harness_event(&st3::harness_events::Publication {
                    runtime_incarnation: prompt.runtime_incarnation.clone(),
                    sequence: event.sequence,
                    claim: ClaimInput {
                        subject: SEAT.into(),
                        kind: "harness.prompt".into(),
                        actor: Some(SEAT.into()),
                        fields: BTreeMap::from([
                            ("incarnation_id".into(), json!(prompt.runtime_incarnation)),
                            ("prompt".into(), json!(prompt)),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("fixture-prompt:{}", event.sequence)),
                    },
                })?;
                changed = true;
            }
            st_drivers::harness_events::acknowledge(&native, event.sequence)?;
        }
        if changed {
            state.notify.notify_waiters();
            state.event_notify.send_modify(|index| *index += 1);
        }
        if Instant::now() >= end {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stopped.store(true, Ordering::SeqCst);
    let output = hook.join().expect("fixture hook thread failed")?;
    let native_output = if output.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&output)?
    };
    println!(
        "{}",
        json!({"native_output":native_output,"fixture_finished":true})
    );
    server.abort();
    history.abort();
    Ok(())
}
