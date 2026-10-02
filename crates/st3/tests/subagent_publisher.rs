//! A seat driver records its harness's subagents from the ledger on the seat: appearances with
//! their description and session, ends with tokens, and a Codex subagent's tokens in its parent's
//! usage. These run against the real API on a private socket; the seat-level proof is
//! `subagents_seat`.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use st_drivers::subagents::{self as ledger, Tokens};
use st3::api::AppState;
use st3::client::{Client, Endpoint};
use st3::store::Store;
use tokio::sync::{Notify, watch};

const CODEX_SEAT: &str = "agent/example/codex";
const CLAUDE_SEAT: &str = "agent/example/claude";

async fn serve(
    root: &Path,
) -> (
    Arc<Store>,
    Client,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let store = Arc::new(Store::open(&root.join("graph.db"), "publisher-test").unwrap());
    let state = AppState {
        store: store.clone(),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "publisher-test".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    };
    let socket = root.join("st3.sock");
    let path = socket.clone();
    let server =
        tokio::spawn(async move { st3::api::serve_unix(&path, st3::api::router(state)).await });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (store, Client::new(Endpoint::Unix(socket)), server)
}

fn fields(store: &Store, seat: &str, kind: &str) -> Vec<Value> {
    store
        .claims_for(seat, Some(kind))
        .unwrap()
        .into_iter()
        .map(|claim| claim.body["fields"].clone())
        .collect()
}

fn activity(kind: &str, thread: &str) -> Value {
    json!({
        "method": "item/completed",
        "params": {
            "threadId": "parent-thread",
            "item": {
                "type": "subAgentActivity", "id": format!("call-{kind}"), "kind": kind,
                "agentThreadId": thread, "agentPath": "/root/review_docs",
            },
        },
    })
}

fn write_rollout(codex_home: &Path, thread: &str, totals: &[(u64, u64, u64)]) {
    let directory = codex_home.join("sessions/2026/10/02");
    std::fs::create_dir_all(&directory).unwrap();
    let mut lines =
        vec![json!({"type": "turn_context", "payload": {"model": "gpt-example"}}).to_string()];
    for (input, cached, output) in totals {
        lines.push(
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {
                    "input_tokens": input, "cached_input_tokens": cached,
                    "output_tokens": output, "total_tokens": input + output,
                },
            }}})
            .to_string(),
        );
    }
    std::fs::write(
        directory.join(format!("rollout-2026-10-02T10-00-00-{thread}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();
}

/// Ends become due for their tokens once the run has settled.
fn settled(agent_dir: &Path) {
    ledger::update(agent_dir, |ledger| {
        for ended in &mut ledger.ended {
            ended.ended_at_ms = ended.ended_at_ms.saturating_sub(5_000);
        }
    })
    .unwrap();
}

#[tokio::test]
async fn a_codex_subagent_run_ends_with_its_own_tokens_in_the_parents_usage() {
    let root = tempfile::tempdir().unwrap();
    let (store, client, server) = serve(root.path()).await;
    let agent_dir = root.path().join("agent");
    let codex_home = root.path().join("codex");
    // The parent's own responses name the account that pays.
    st_drivers::harness_timeline::Writer::new(&agent_dir, "codex", "codex-incarnation")
        .with_account(Some("codex/aaaaaaaaaaaaaaaa".into()))
        .append(
            "codex:usage:turn-1:10",
            st_drivers::harness_timeline::Role::System,
            st_drivers::harness_timeline::EntryType::Usage,
            json!({"semantics": "response", "model": "gpt-example", "total_tokens": 10}),
            true,
        )
        .unwrap();
    let mut publisher = st3::subagents::Publisher::start(
        CODEX_SEAT,
        "codex",
        "incarnation-1",
        &agent_dir,
        ledger::now_ms(),
    )
    .with_homes(None, Some(codex_home.clone()));
    publisher.set_timeline_incarnation(Some("codex-incarnation".into()));

    ledger::observe_codex(&agent_dir, &activity("started", "child"), "parent-thread").unwrap();
    publisher.tick(&client).await.unwrap();
    let appeared = fields(&store, CODEX_SEAT, "subagent.appeared");
    assert_eq!(appeared.len(), 1);
    assert_eq!(appeared[0]["subagent_id"], "child");
    assert_eq!(appeared[0]["driver"], "codex");
    assert_eq!(appeared[0]["description"], "review docs");
    assert_eq!(appeared[0]["session_id"], "parent-thread");
    assert_eq!(appeared[0]["incarnation_id"], "incarnation-1");

    write_rollout(&codex_home, "child", &[(100, 60, 10), (300, 200, 25)]);
    ledger::observe_codex(&agent_dir, &activity("completed", "child"), "parent-thread").unwrap();
    // The end waits for the run's last token count to settle.
    publisher.tick(&client).await.unwrap();
    assert!(fields(&store, CODEX_SEAT, "subagent.ended").is_empty());
    settled(&agent_dir);
    publisher.tick(&client).await.unwrap();
    let ended = fields(&store, CODEX_SEAT, "subagent.ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["outcome"], "completed");
    assert_eq!(ended[0]["input_tokens"], 100);
    assert_eq!(ended[0]["cached_tokens"], 200);
    assert_eq!(ended[0]["output_tokens"], 25);
    assert_eq!(ended[0]["total_tokens"], 325);
    assert!(ledger::read(&agent_dir).ended.is_empty());

    // A follow-up task is a second run that counts only its own tokens.
    ledger::observe_codex(
        &agent_dir,
        &activity("interacted", "child"),
        "parent-thread",
    )
    .unwrap();
    write_rollout(
        &codex_home,
        "child",
        &[(100, 60, 10), (300, 200, 25), (500, 350, 40)],
    );
    ledger::observe_codex(&agent_dir, &activity("completed", "child"), "parent-thread").unwrap();
    settled(&agent_dir);
    publisher.tick(&client).await.unwrap();
    let ended = fields(&store, CODEX_SEAT, "subagent.ended");
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[1]["subagent_id"], "child#2");
    assert_eq!(ended[1]["total_tokens"], 215);
    assert_eq!(ended[1]["output_tokens"], 15);

    let record = st_drivers::harness_timeline::read(&st_drivers::harness_timeline::timeline_path(
        &agent_dir,
    ))
    .unwrap();
    let usage = record
        .operations
        .iter()
        .filter(|operation| {
            operation.body["turn_id"]
                .as_str()
                .is_some_and(|turn| turn.starts_with("subagent:"))
        })
        .map(|operation| {
            (
                operation.body["turn_id"].as_str().unwrap().to_owned(),
                operation.body["total_tokens"].as_u64().unwrap(),
                operation.body["account"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        usage,
        [
            (
                "subagent:child".into(),
                325,
                "codex/aaaaaaaaaaaaaaaa".into()
            ),
            (
                "subagent:child#2".into(),
                215,
                "codex/aaaaaaaaaaaaaaaa".into()
            ),
        ]
    );
    assert_eq!(
        ledger::read(&agent_dir).counted["child"],
        Tokens {
            input_tokens: 150,
            output_tokens: 40,
            cache_write_tokens: 0,
            cached_tokens: 350,
            total_tokens: 540,
        }
    );
    server.abort();
}

#[tokio::test]
async fn a_new_driver_incarnation_ends_what_the_last_harness_left_running() {
    let root = tempfile::tempdir().unwrap();
    let (store, client, server) = serve(root.path()).await;
    let agent_dir = root.path().join("agent");
    let started = ledger::now_ms();
    let mut first = st3::subagents::Publisher::start(
        CLAUDE_SEAT,
        "claude",
        "incarnation-1",
        &agent_dir,
        started,
    );
    ledger::observe_claude(
        &agent_dir,
        "SubagentStart",
        &json!({"session_id": "session-1", "agent_id": "a1", "agent_type": "Explore"}),
    )
    .unwrap();
    first.tick(&client).await.unwrap();
    assert_eq!(fields(&store, CLAUDE_SEAT, "subagent.appeared").len(), 1);

    // The driver is replaced in place: same incarnation, the subagent runs on.
    let mut replaced = st3::subagents::Publisher::start(
        CLAUDE_SEAT,
        "claude",
        "incarnation-1",
        &agent_dir,
        started + 10_000,
    );
    replaced.tick(&client).await.unwrap();
    assert!(fields(&store, CLAUDE_SEAT, "subagent.ended").is_empty());
    assert_eq!(fields(&store, CLAUDE_SEAT, "subagent.appeared").len(), 1);

    // A new incarnation means a new harness: the old one's subagent died with it.
    let mut next = st3::subagents::Publisher::start(
        CLAUDE_SEAT,
        "claude",
        "incarnation-2",
        &agent_dir,
        started + 20_000,
    );
    next.tick(&client).await.unwrap();
    let ended = fields(&store, CLAUDE_SEAT, "subagent.ended");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["outcome"], "harness-exited");
    assert_eq!(ended[0]["reason"], "its harness restarted");
    assert_eq!(fields(&store, CLAUDE_SEAT, "subagent.appeared").len(), 1);
    assert!(ledger::read(&agent_dir).ended.is_empty());

    // A subagent that started and ended between two ticks still appears and ends.
    ledger::observe_claude(
        &agent_dir,
        "SubagentStart",
        &json!({"session_id": "session-1", "agent_id": "quick", "agent_type": "Explore"}),
    )
    .unwrap();
    ledger::observe_claude(
        &agent_dir,
        "SubagentStop",
        &json!({"session_id": "session-1", "agent_id": "quick", "agent_type": "Explore"}),
    )
    .unwrap();
    next.tick(&client).await.unwrap();
    let ended = fields(&store, CLAUDE_SEAT, "subagent.ended");
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[1]["subagent_id"], "quick");
    assert_eq!(ended[1]["outcome"], "completed");
    server.abort();
}
