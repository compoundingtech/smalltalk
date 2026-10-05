use axum::{
    Json, Router,
    extract::ws::{Message, WebSocketUpgrade},
    routing::{get, post},
};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use st3_client::{Client, Resource, Snapshot};
use st3_client_tui::{App, send};
use st3_feed::{Command, Update, Window};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

fn snapshot() -> Snapshot {
    serde_json::from_value(json!({"id":"snapshot/demo/1", "host_id":"host/demo", "store_index":1, "projection_version":"client-projection.v0", "created_at":"2026-10-01T12:00:00Z"})).unwrap()
}
fn agent(id: &str) -> Resource {
    serde_json::from_value(json!({"kind":"agent", "id":format!("agent/{id}"), "revision":"demo", "updated_at":"2026-10-01T12:00:00Z", "name":id, "state":"running", "reachability":"local"})).unwrap()
}
fn agents(app: &mut App, ids: &[&str]) -> Option<Command> {
    app.update(Update::Window {
        window: Window::Agents,
        snapshot: snapshot(),
        items: ids.iter().map(|id| agent(id)).collect(),
        has_more: false,
    })
}

#[test]
fn selection_survives_reordering_and_offline_is_read_only() {
    let mut app = App::default();
    assert!(
        matches!(agents(&mut app, &["writer","reviewer"]), Some(Command::Converse { targets }) if targets == ["agent/writer"])
    );
    app.select(1);
    assert!(agents(&mut app, &["reviewer", "writer"]).is_none());
    assert_eq!(app.selected.as_deref(), Some("agent/reviewer"));
    app.draft = "Hello".into();
    app.update(Update::Offline("network down".into()));
    assert!(app.submit().is_none());
    assert_eq!(app.agents.len(), 2);
    let mut terminal = Terminal::new(TestBackend::new(85, 18)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(text.contains("Offline"));
    assert!(text.contains("Agents · stale"));
    assert!(text.contains("reviewer"));
}

#[test]
fn failed_send_retries_same_recipient_body_and_idempotency_key() {
    let mut app = App::default();
    agents(&mut app, &["writer", "reviewer"]);
    app.draft = "Please check the build.".into();
    let first = app.submit().unwrap();
    assert!(app.select(1).is_none());
    assert!(app.submit().is_none());
    app.sent(Err(st3_client::ClientError::Transport(
        "response lost".into(),
    )));
    // Removing the recipient from a bounded window must not retarget an uncertain send.
    agents(&mut app, &["reviewer"]);
    let retry = app.submit().unwrap();
    assert_eq!(
        (&first.id, &first.key, &first.to, &first.body),
        (&retry.id, &retry.key, &retry.to, &retry.body)
    );
    assert_eq!(app.selected.as_deref(), Some("agent/writer"));
    app.sent(Ok("Accepted".into()));
    assert!(app.draft.is_empty());
    assert!(app.pending.is_none());
}

#[tokio::test]
async fn real_feed_follows_and_typed_client_submits_fenced_send() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let actions = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = actions.clone();
        let router = Router::new()
            .route("/v1/client/capabilities", get(|| async { Json(json!({
                "api_version":"st3.client.v0", "request_id":"request/caps", "snapshot":snapshot(),
                "value":{"kind":"capabilities", "session_actor":"person/avery", "transport":"fabric-loopback", "capabilities":[], "limits":{"max_page_items":200,"max_event_items":200,"max_response_bytes":8388608,"max_wait_ms":1000}, "event_cursor":"event/1","oldest_event_cursor":"event/1","schemas":[]}
            })) }))
            .route("/v1/client/actions", post(move |Json(body): Json<Value>| { let actions = captured.clone(); async move {
                actions.lock().unwrap().push(body.clone());
                Json(json!({"api_version":"st3.client.v0","request_id":"request/send","snapshot":snapshot(),"value":{"kind":"action-result","action_id":body["id"],"operation_id":"operation/demo","status":"completed","affected_ids":["message/demo"],"snapshot_id":"snapshot/demo/1"}}))
            }}))
            .route("/v1/client/collections/stream", get(|upgrade: WebSocketUpgrade| async {
                upgrade.protocols(["st3.client.collections.v0"]).on_upgrade(|mut socket| async move {
                    while let Some(Ok(Message::Text(text))) = socket.recv().await {
                        let command: Value = serde_json::from_str(&text).unwrap();
                        if command["kind"] != "subscribe" { continue; }
                        let frame = if command["collection"] == "conversation" {
                            assert_eq!(command["conversation"], "agent/writer");
                            let entries: Value = serde_json::from_str(include_str!("../../../fixtures/clients/transcripts/deliveries.json")).unwrap();
                            json!({"kind":"conversation","id":command["id"],"session_id":"session/demo","replace":true,"has_more":false,"items":entries})
                        } else {
                            let items = if command["collection"] == "agents" { vec![agent("writer")] } else { vec![] };
                            let order = if items.is_empty() { vec![] } else { vec!["agent/writer"] };
                            json!({"kind":"snapshot","id":command["id"],"snapshot":snapshot(),"items":items,"order":order,"has_more":false})
                        };
                        socket.send(Message::Text(frame.to_string().into())).await.unwrap();
                    }
                })
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::fabric_loopback(format!("http://{}", listener.local_addr().unwrap()), "example-pairing");
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let (updates, incoming) = mpsc::channel();
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let feed = tokio::spawn(st3_feed::run(client, updates, receiver));
        let mut app = App::default();
        while app.timeline.items.is_empty() {
            while let Ok(update) = incoming.try_recv() {
                if let Some(command) = app.update(update) { commands.send(command).unwrap(); }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let rendered = terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect::<String>();
        assert!(rendered.contains("Two readers still use the old keys; both are fixed."));
        assert_eq!(app.selected.as_deref(), Some("agent/writer"));
        assert!(app.live);
        assert!(!app.timeline.items.is_empty());
        app.draft = "Please check the build.".into();
        let request = app.submit().unwrap();
        let result = send(app.client.as_ref().unwrap(), &request).await.unwrap();
        assert_eq!(result, "Accepted: operation/demo");
        app.sent(Ok(result));
        let posted = actions.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0]["type"], "message.send");
        assert_eq!(posted[0]["parameters"]["to"], "agent/writer");
        assert_eq!(posted[0]["parameters"]["content"], "Please check the build.");
        assert_eq!(posted[0]["fence"]["snapshot_id"], "snapshot/demo/1");
        assert_eq!(posted[0]["idempotency_key"], request.key);
        drop(posted);
        drop(commands); feed.abort(); server.abort();
    }).await.expect("example feed/send proof timed out");
}
