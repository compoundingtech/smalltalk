use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use st3::api::AppState;
use st3::model::{AttentionRequest, ClaimInput};
use st3::store::Store;
use st3_client::{
    AttentionResolveParameters, Capabilities, Client, ClientError, CollectionEvent, Envelope,
    ErrorCode, Fence, LaunchVariantParameters, PairingBegin, PairingComplete, Resource,
    TargetParameters, TerminalAttachment, TerminalColor, TerminalInputMode,
    TerminalInputParameters, TerminalResizeParameters, TerminalRun, TerminalScreen, TerminalStream,
    TimelineBody, TimelineUsageSemantics,
};
use tokio::sync::{Notify, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

fn state(root: &Path, name: &str) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory(name).unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: name.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary: root.join("unused-pty"),
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

fn publish_terminal(state: &AppState, incarnation: &str) {
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/terminal-demo".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/terminal-demo".into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    Value::String("terminal-demo-runtime".into()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
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
            subject: "agent/terminal-demo".into(),
            kind: "harness.timeline".into(),
            actor: Some("agent/terminal-demo".into()),
            fields: BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                (
                    "entry_id".into(),
                    Value::String(format!("timeline-entry/usage-{incarnation}")),
                ),
                ("sequence".into(), Value::from(1)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("system".into())),
                ("entry_type".into(), Value::String("usage".into())),
                ("final".into(), Value::Bool(true)),
                ("body".into(), serde_json::json!({"total_tokens":7})),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("timeline-usage-{incarnation}")),
        })
        .unwrap();
    // The reconciler announces every observation it records; so does this stand-in.
    state
        .event_notify
        .send_modify(|generation| *generation = generation.saturating_add(1));
}

/// A PTY session socket that answers a read-only PEEK the way `pty` does: the geometry, the
/// replayed screen, then live output until the process exits.
#[derive(Clone)]
struct FakePty {
    replay: Arc<std::sync::Mutex<Vec<u8>>>,
    output: tokio::sync::broadcast::Sender<Option<Vec<u8>>>,
}

fn pty_packet(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![kind];
    packet.extend((payload.len() as u32).to_be_bytes());
    packet.extend(payload);
    packet
}

impl FakePty {
    fn start(state: &AppState, rows: u16, columns: u16) -> Self {
        std::fs::create_dir_all(&state.pty_root).unwrap();
        let listener =
            tokio::net::UnixListener::bind(state.pty_root.join("terminal-demo-runtime.sock"))
                .unwrap();
        let pty = Self {
            replay: Arc::new(std::sync::Mutex::new(b"terminal ready\r\n$ ".to_vec())),
            output: tokio::sync::broadcast::channel(4_096).0,
        };
        let session = pty.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            while let Ok((mut stream, _)) = listener.accept().await {
                let (replay, mut output) = {
                    let replay = session.replay.lock().unwrap();
                    (replay.clone(), session.output.subscribe())
                };
                tokio::spawn(async move {
                    let mut peek = [0_u8; 6];
                    if stream.read_exact(&mut peek).await.is_err() || peek[0] != 6 {
                        return;
                    }
                    let mut geometry = rows.to_be_bytes().to_vec();
                    geometry.extend(columns.to_be_bytes());
                    if stream.write_all(&pty_packet(10, &geometry)).await.is_err()
                        || stream.write_all(&pty_packet(5, &replay)).await.is_err()
                    {
                        return;
                    }
                    while let Ok(output) = output.recv().await {
                        let exited = output.is_none();
                        let packet = match output {
                            Some(bytes) => pty_packet(0, &bytes),
                            None => pty_packet(4, &0_i32.to_be_bytes()),
                        };
                        if stream.write_all(&packet).await.is_err() || exited {
                            return;
                        }
                    }
                });
            }
        });
        pty
    }

    fn write(&self, bytes: &[u8]) {
        let mut replay = self.replay.lock().unwrap();
        replay.extend_from_slice(bytes);
        let _ = self.output.send(Some(bytes.to_vec()));
    }

    fn exit(&self) {
        let _ = self.output.send(None);
    }
}

async fn first_screen(
    client: &Client,
    attachment: &TerminalAttachment,
    incarnation: &str,
) -> Result<(TerminalStream, Envelope<TerminalScreen>), ClientError> {
    let mut stream = client
        .terminal_stream(
            &attachment.terminal_id,
            Some(incarnation),
            attachment.stream_capability.as_deref().unwrap(),
        )
        .await?;
    let screen = stream
        .next()
        .await?
        .ok_or_else(|| ClientError::Protocol("the stream closed before its first screen".into()))?;
    Ok((stream, screen))
}

async fn serve_terminal_state(
    name: &str,
    rows: u16,
    columns: u16,
) -> (
    tempfile::TempDir,
    AppState,
    FakePty,
    Client,
    tokio::task::AbortHandle,
) {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = state(root.path(), name);
    publish_terminal(&state, "terminal-demo-runtime:i1");
    let pty = FakePty::start(&state, rows, columns);
    let app = st3::api::router(state.clone());
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    wait_for_socket(&socket).await;
    (
        root,
        state,
        pty,
        Client::unix_as(&socket, "person/avery"),
        server.abort_handle(),
    )
}

#[tokio::test]
async fn nested_launch_ids_reach_detail_and_child_routes() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = state(root.path(), "launch-routing");
    let request = state
        .store
        .put_document(
            "doc/planning/request",
            b"Plan a release",
            &None,
            "launch-request",
        )
        .unwrap();
    for id in [
        "planning/fleet/harbor/release/one",
        "launch/fleet/harbor/release/two",
    ] {
        state
            .store
            .create_planning_session(
                id,
                "release",
                &format!("{}@{}", request.name, request.hash),
                "/work/release",
                "person/avery",
                "agent/launch-routing.planner",
                &Default::default(),
                None,
                None,
            )
            .unwrap();
    }
    let app = st3::api::router(state);
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    wait_for_socket(&socket).await;
    let client = Client::unix(&socket);
    for id in [
        "launch/planning/fleet/harbor/release/one",
        "launch/launch/fleet/harbor/release/two",
    ] {
        assert_eq!(client.launches_get(id).await.unwrap().value.header().id, id);
        assert!(
            client
                .launch_variants_list(id, None, None)
                .await
                .unwrap()
                .value
                .items
                .is_empty()
        );
    }
    server.abort();
}

#[tokio::test]
async fn an_empty_launch_404_explains_that_the_launch_no_longer_exists() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new()).await.unwrap();
    });
    let client = Client::fabric_pairing(format!("http://{address}"));
    let error = client
        .launches_get("launch/planning/missing/one")
        .await
        .unwrap_err();
    match error {
        ClientError::Api(ErrorCode::NotFound, message, envelope) => {
            assert_eq!(message, "this launch no longer exists");
            assert!(!envelope.retryable);
        }
        other => panic!("missing launch must be understandable: {other}"),
    }
    server.abort();
}

#[tokio::test]
async fn terminal_stream_sends_changed_screens_and_nothing_while_idle() {
    let (_root, state, pty, client, server) =
        serve_terminal_state("client-stream-changes", 24, 80).await;
    let attachment = attach_terminal(&client, "stream-changes").await;
    let (mut stream, first) = first_screen(&client, &attachment, "terminal-demo-runtime:i1")
        .await
        .unwrap();
    assert_eq!(first.value.lines[0].text, "terminal ready");
    assert!(!first.value.revision.is_empty());

    assert!(
        tokio::time::timeout(Duration::from_millis(1_500), stream.next())
            .await
            .is_err(),
        "an idle terminal must send nothing"
    );

    pty.write(b"\x1b[1;32mecho\x1b[0m hi");
    let changed = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a change must arrive promptly")
        .unwrap()
        .unwrap();
    assert_eq!(changed.value.lines[1].text, "$ echo hi");
    assert_eq!(
        changed.value.lines[1].runs[1],
        TerminalRun {
            text: "echo".into(),
            fg: Some(TerminalColor::Palette(2)),
            bold: true,
            ..TerminalRun::default()
        }
    );
    assert_ne!(changed.value.revision, first.value.revision);

    // Output that leaves the screen as it was is not a change.
    pty.write(b"\x08i");
    assert!(
        tokio::time::timeout(Duration::from_millis(1_000), stream.next())
            .await
            .is_err(),
        "an unchanged screen must not be sent again"
    );

    publish_terminal(&state, "terminal-demo-runtime:i2");
    let ended = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a changed incarnation must end the stream promptly");
    assert!(
        matches!(ended, Err(ClientError::Api(ErrorCode::StaleFence, _, _))),
        "a changed incarnation must close the stream with stale-fence: {ended:?}"
    );

    let attachment = attach_terminal(&client, "stream-exit").await;
    let (mut stream, reconnected) =
        first_screen(&client, &attachment, "terminal-demo-runtime:i2")
            .await
            .unwrap();
    assert_eq!(reconnected.value.lines[1].text, "$ echo hi");
    pty.exit();
    let ended = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an exited terminal must end the stream promptly");
    assert!(
        matches!(ended, Err(ClientError::Api(ErrorCode::StaleFence, _, _))),
        "an exited terminal must close the stream with stale-fence: {ended:?}"
    );
    server.abort();
}

/// The next collection event, or a panic naming what did not arrive.
async fn next_collection_event(
    stream: &mut st3_client::CollectionStream,
    waiting_for: &str,
) -> CollectionEvent {
    tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap_or_else(|_| panic!("{waiting_for} did not arrive"))
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_terminal_rides_the_collection_socket_and_its_end_leaves_the_rest() {
    let (_root, state, pty, client, server) =
        serve_terminal_state("client-collection-terminal", 24, 80).await;
    let attachment = attach_terminal(&client, "collection-terminal").await;
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe("agents", "agents", 20, None, None)
        .await
        .unwrap();
    assert!(matches!(
        next_collection_event(&mut stream, "the agents snapshot").await,
        CollectionEvent::Snapshot { id, .. } if id == "agents"
    ));
    stream
        .subscribe_terminal(
            "screen",
            &attachment.terminal_id,
            Some("terminal-demo-runtime:i1"),
            attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();
    let first = loop {
        match next_collection_event(&mut stream, "the first screen").await {
            CollectionEvent::Screen { id, screen } if id == "screen" => break screen,
            // The attach and the observation behind it may still move the agents window.
            CollectionEvent::Changes { id, .. } if id == "agents" => {}
            other => panic!("expected the first screen, got {other:?}"),
        }
    };
    assert_eq!(first.value.lines[0].text, "terminal ready");

    // The capability is single use: a second subscription with it is refused on its own.
    stream
        .subscribe_terminal(
            "again",
            &attachment.terminal_id,
            Some("terminal-demo-runtime:i1"),
            attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        next_collection_event(&mut stream, "the refused reuse").await,
        CollectionEvent::Error { id, .. } if id == "again"
    ));

    assert!(
        tokio::time::timeout(Duration::from_millis(1_500), stream.next_event())
            .await
            .is_err(),
        "an idle terminal and an unchanged collection must send nothing"
    );

    pty.write(b"\x1b[1;32mecho\x1b[0m hi");
    let changed = match next_collection_event(&mut stream, "the changed screen").await {
        CollectionEvent::Screen { id, screen } if id == "screen" => screen,
        other => panic!("expected the changed screen, got {other:?}"),
    };
    assert_eq!(changed.value.lines[1].text, "$ echo hi");
    assert_ne!(changed.value.revision, first.value.revision);

    // A new incarnation ends the terminal subscription with stale-fence. The agents
    // subscription on the same socket sees the observation and keeps going.
    publish_terminal(&state, "terminal-demo-runtime:i2");
    let mut ended = false;
    let mut agents_changed = false;
    while !(ended && agents_changed) {
        match next_collection_event(&mut stream, "the stale-fence end and the agents change").await
        {
            CollectionEvent::Error { id, code, .. } if id == "screen" => {
                assert_eq!(code, Some(ErrorCode::StaleFence));
                ended = true;
            }
            CollectionEvent::Changes { id, .. } if id == "agents" => agents_changed = true,
            other => panic!("unexpected frame after a new incarnation: {other:?}"),
        }
    }
    pty.write(b"!");
    assert!(
        tokio::time::timeout(Duration::from_millis(1_000), stream.next_event())
            .await
            .is_err(),
        "an ended terminal subscription must send nothing more"
    );
    server.abort();
}

#[tokio::test]
async fn an_agents_conversation_rides_the_collection_socket_with_its_small_talk() {
    let (_root, state, _pty, client, server) =
        serve_terminal_state("client-collection-conversation", 24, 80).await;
    let mut stream = client.collection_stream().await.unwrap();
    stream
        .subscribe_conversation("chat", "agent/terminal-demo")
        .await
        .unwrap();
    let session_id = match next_collection_event(&mut stream, "the conversation page").await {
        CollectionEvent::Conversation {
            id,
            session_id,
            replace: true,
            ..
        } if id == "chat" => session_id,
        other => panic!("expected the conversation page, got {other:?}"),
    };
    assert!(session_id.starts_with("session/"), "{session_id}");

    // Small Talk to the agent that names no session is part of its conversation: st joins it.
    state
        .store
        .append_claim(&ClaimInput {
            subject: "message/conversation-socket".into(),
            kind: "message.sent".into(),
            actor: Some("agent/terminal-peer".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("agent/terminal-peer".into())),
                ("to".into(), Value::String("agent/terminal-demo".into())),
                ("content".into(), Value::String("hello from a peer".into())),
                ("title".into(), Value::String("A question".into())),
                ("status".into(), Value::String("sent".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let items = match next_collection_event(&mut stream, "the new message").await {
        CollectionEvent::Conversation {
            id,
            replace: false,
            items,
            ..
        } if id == "chat" => items,
        other => panic!("expected the new message, got {other:?}"),
    };
    let header = items
        .iter()
        .find_map(|item| match &item.body {
            st3_client::TimelineBody::Message(message) => Some(message),
            _ => None,
        })
        .expect("the message header");
    assert_eq!(header.from.as_deref(), Some("agent/terminal-peer"));
    assert_eq!(header.to.as_deref(), Some("agent/terminal-demo"));
    assert_eq!(header.title.as_deref(), Some("A question"));
    assert!(items.iter().any(|item| matches!(
        &item.body,
        st3_client::TimelineBody::Content(content) if content.text.as_deref() == Some("hello from a peer")
    )));

    assert!(
        tokio::time::timeout(Duration::from_millis(1_500), stream.next_event())
            .await
            .is_err(),
        "an idle conversation must send nothing"
    );
    stream.unsubscribe("chat").await.unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "message/conversation-socket-later".into(),
            kind: "message.sent".into(),
            actor: Some("agent/terminal-peer".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("agent/terminal-peer".into())),
                ("to".into(), Value::String("agent/terminal-demo".into())),
                ("content".into(), Value::String("after unsubscribe".into())),
                ("status".into(), Value::String("sent".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1_000), stream.next_event())
            .await
            .is_err(),
        "an unsubscribed conversation must send nothing more"
    );
    server.abort();
}

#[tokio::test]
async fn a_slow_terminal_client_gets_the_latest_screen_not_a_backlog() {
    const ROWS: usize = 100;
    const CHANGES: usize = 200;
    let (_root, _state, pty, client, server) =
        serve_terminal_state("client-stream-slow", ROWS as u16, 250).await;
    let attachment = attach_terminal(&client, "stream-slow").await;
    let (mut stream, _) = first_screen(&client, &attachment, "terminal-demo-runtime:i1")
        .await
        .unwrap();
    // Fill every row with the change number, and read nothing while the terminal keeps changing
    // for four seconds: long enough to fill the socket buffers between server and client.
    for change in 0..CHANGES {
        let row = format!("{change:04} ").repeat(49);
        let screen = vec![row; ROWS].join("\r\n");
        pty.write(format!("\x1b[H{screen}").as_bytes());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut received = Vec::new();
    while let Ok(screen) = tokio::time::timeout(Duration::from_millis(1_500), stream.next()).await {
        received.push(screen.unwrap().unwrap());
    }
    let last = received.last().expect("the latest screen arrives");
    assert!(
        last.value.lines[0]
            .text
            .starts_with(&format!("{:04}", CHANGES - 1)),
        "the last screen must be the latest one: {:?}",
        &last.value.lines[0].text[..10]
    );
    let changes = received
        .iter()
        .map(|screen| screen.value.lines[0].text[..4].parse::<usize>().unwrap())
        .collect::<Vec<_>>();
    assert!(
        changes.windows(2).all(|pair| pair[0] < pair[1]),
        "screens only move forward: {changes:?}"
    );
    // The watcher publishes at most one screen per 100 ms, about 40 in four seconds. A client
    // that fell behind receives what its socket buffers held plus the latest, never every one.
    assert!(
        received.len() <= 20,
        "a slow client must not receive a backlog of {} screens: {changes:?}",
        received.len()
    );
    server.abort();
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("Unix server socket {} did not appear", socket.display());
}

fn assert_private_socket(socket: &Path) {
    let mode = std::fs::metadata(socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o600,
        "Unix socket {} must be accessible only to its owner",
        socket.display()
    );
}

async fn unix_status(socket: &Path, path: &str, credential: Option<&str>) -> StatusCode {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "localhost");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    sender
        .send_request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn attach_terminal(client: &Client, suffix: &str) -> TerminalAttachment {
    let runtimes = client.runtimes_list(None, None, false).await.unwrap();
    let runtime = runtimes
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Runtime(runtime) if runtime.terminal_id.is_some() => Some(runtime),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "terminal-capable runtime advertised by the server: {:?}",
                runtimes.value.items
            )
        });
    let terminal_id = runtime.terminal_id.as_deref().unwrap();
    let incarnation = runtime.incarnation_id.as_deref().unwrap();
    let terminal_sequence = runtime.terminal_sequence.unwrap();
    assert_eq!(runtime.terminal_access.as_ref().unwrap().read, "granted");
    client
        .terminal_attach(
            format!("action/terminal-attach-{suffix}"),
            format!("terminal-attach-{suffix}-00000001"),
            Fence {
                snapshot_id: runtimes.snapshot.id,
                runtime_incarnation: Some(incarnation.into()),
                terminal_sequence: Some(terminal_sequence),
                ..Fence::default()
            },
            TargetParameters {
                target_id: terminal_id.into(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap()
        .value
        .terminal_attachment
        .expect("terminal.attach result")
}

async fn detach_terminal(client: &Client, attachment: &TerminalAttachment, suffix: &str) {
    let capabilities = client.capabilities().await.unwrap();
    client
        .terminal_detach(
            format!("action/terminal-detach-{suffix}"),
            format!("terminal-detach-{suffix}-0000001"),
            Fence {
                snapshot_id: capabilities.snapshot.id,
                runtime_incarnation: Some(attachment.runtime_incarnation.clone()),
                terminal_sequence: Some(capabilities.snapshot.store_index),
                ..Fence::default()
            },
            TargetParameters {
                target_id: attachment.attachment_id.clone(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn generated_client_conforms_over_the_real_unix_transport() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let state = state(root.path(), "client-unix");
    publish_terminal(&state, "terminal-demo-runtime:i1");
    let _pty = FakePty::start(&state, 24, 80);
    state
        .store
        .request_attention(
            "attention/ada-client-proof",
            &AttentionRequest {
                reviewer: "person/ada".into(),
                title: "Client proof".into(),
                reason: "Resolve through the generated client".into(),
                severity: "error".into(),
                targets: vec!["agent/terminal-demo".into()],
                actor: "daemon/runtime".into(),
                idempotency_key: "attention-ada-client-proof".into(),
            },
        )
        .unwrap();
    state
        .store
        .request_attention(
            "attention/alex-client-proof",
            &AttentionRequest {
                reviewer: "person/alex".into(),
                title: "Other person's attention".into(),
                reason: "Must not resolve as Ada".into(),
                severity: "warning".into(),
                targets: Vec::new(),
                actor: "daemon/runtime".into(),
                idempotency_key: "attention-alex-client-proof".into(),
            },
        )
        .unwrap();
    let app = st3::api::router(state.clone());
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    wait_for_socket(&socket).await;

    let client = Client::unix_as(&socket, "person/ada");
    let read_only = Client::unix(&socket);
    assert_eq!(
        read_only.capabilities().await.unwrap().value.session_actor,
        "client/local/read-only"
    );
    assert!(
        read_only
            .pairing_begin(&PairingBegin {
                api_version: st3_client::API_VERSION.into(),
                device_name: "Unattributed device".into(),
                person_id: "person/ada".into(),
                full_control: None,
            })
            .await
            .is_err(),
        "plain Unix clients cannot infer pairing authority"
    );
    let capabilities = client.capabilities().await.unwrap();
    assert_eq!(
        capabilities.value.transport,
        st3_client::TransportKind::Unix
    );
    assert_eq!(capabilities.value.session_actor, "person/ada");
    let work = client.work_list(None, Some(7), false).await.unwrap();
    assert_eq!(work.value.collection, "work");
    assert_eq!(work.value.page.limit, 7);
    let events = client.events(None, Some(10), Some(0)).await.unwrap();
    assert!(!events.value.has_more);

    let attention_page = client.attention_list(None, None, false).await.unwrap();
    let attention = attention_page
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Attention(attention)
                if attention.header.id == "attention/ada-client-proof" =>
            {
                Some(attention)
            }
            _ => None,
        })
        .expect("person/ada attention in generated client page");
    let resolve_fence = Fence {
        snapshot_id: attention_page.snapshot.id.clone(),
        subject_revisions: BTreeMap::from([(
            attention.header.id.clone(),
            attention.header.revision.clone(),
        )]),
        ..Fence::default()
    };
    let resolve_parameters = AttentionResolveParameters {
        attention_id: attention.header.id.clone(),
        outcome: "resolved".into(),
        reason: Some("generated client proof".into()),
    };
    assert!(
        read_only
            .attention_resolve(
                "action/attention-resolve-read-only",
                "attention-resolve-read-only-0001",
                resolve_fence.clone(),
                resolve_parameters.clone(),
            )
            .await
            .is_err(),
        "plain Unix clients cannot resolve person attention"
    );
    assert!(
        read_only
            .launch_approve(
                "action/launch-approve-without-person",
                "launch-approve-no-person-0001",
                Fence {
                    snapshot_id: attention_page.snapshot.id.clone(),
                    ..Fence::default()
                },
                LaunchVariantParameters {
                    launch_id: "launch/example".into(),
                    variant_id: "launch-variant/example/default".into(),
                },
            )
            .await
            .is_err(),
        "plain Unix clients cannot approve launches"
    );
    let sessions = read_only.sessions_list(None, None, false).await.unwrap();
    let session = sessions
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Session(session) if session.owner_id == "agent/terminal-demo" => {
                Some(session)
            }
            _ => None,
        })
        .expect("the advertised runtime has a typed session resource");
    let timeline = read_only
        .timeline(&session.header.id, None, None)
        .await
        .expect("the generated route strips and encodes the projected session ID");
    let usage = timeline
        .value
        .items
        .iter()
        .find_map(|entry| match &entry.body {
            TimelineBody::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("source usage without semantics still decodes as a typed usage body");
    assert_eq!(usage.semantics, TimelineUsageSemantics::Response);
    assert_eq!(usage.driver, "codex");
    assert_eq!(usage.total_tokens, Some(7));
    client
        .attention_resolve(
            "action/attention-resolve-generated-client",
            "attention-resolve-client-0001",
            resolve_fence,
            resolve_parameters,
        )
        .await
        .unwrap();
    assert!(
        state
            .store
            .attention_items(Some("person/ada"))
            .unwrap()
            .is_empty()
    );
    // A person-scoped client cannot enumerate another person's private inbox, but any
    // person can close any item. Use the read-only local projection to obtain the exact
    // cross-person fence, then prove the close is recorded as this client's person.
    let alex_page = read_only.attention_list(None, None, false).await.unwrap();
    let alex = alex_page
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Attention(attention)
                if attention.header.id == "attention/alex-client-proof" =>
            {
                Some(attention)
            }
            _ => None,
        })
        .unwrap();
    client
        .attention_resolve(
            "action/attention-resolve-cross-person",
            "attention-resolve-cross-0001",
            Fence {
                snapshot_id: alex_page.snapshot.id,
                subject_revisions: BTreeMap::from([(
                    alex.header.id.clone(),
                    alex.header.revision.clone(),
                )]),
                ..Fence::default()
            },
            AttentionResolveParameters {
                attention_id: alex.header.id.clone(),
                outcome: "resolved".into(),
                reason: None,
            },
        )
        .await
        .expect("person/ada closes person/alex attention");
    let closed = state
        .store
        .latest_claim("attention/alex-client-proof", Some("attention.resolved"))
        .unwrap()
        .expect("the item was closed");
    assert_eq!(closed.actor.as_deref(), Some("person/ada"));
    assert_eq!(
        closed
            .body
            .pointer("/fields/outcome")
            .and_then(|outcome| outcome.as_str()),
        Some("resolved"),
        "{}",
        closed.body
    );

    let read_only_attachment = attach_terminal(&read_only, "unix-read-only").await;
    let (read_only_stream, read_only_screen) = first_screen(
        &read_only,
        &read_only_attachment,
        &read_only_attachment.runtime_incarnation,
    )
    .await
    .expect("a plain Unix client may consume its read-only viewer capability");
    assert_eq!(read_only_screen.value.lines[0].text, "terminal ready");
    read_only_stream.close().await;
    let read_only_fence = read_only.capabilities().await.unwrap();
    for denied in [
        read_only
            .terminal_input(
                "action/read-only-terminal-input",
                "read-only-terminal-input-0001",
                Fence {
                    snapshot_id: read_only_fence.snapshot.id.clone(),
                    runtime_incarnation: Some(read_only_attachment.runtime_incarnation.clone()),
                    terminal_sequence: Some(read_only_fence.snapshot.store_index),
                    ..Fence::default()
                },
                TerminalInputParameters {
                    terminal_id: read_only_attachment.terminal_id.clone(),
                    mode: TerminalInputMode::Line,
                    value: "must not be written".into(),
                },
            )
            .await,
        read_only
            .terminal_resize(
                "action/read-only-terminal-resize",
                "read-only-terminal-resize-0001",
                Fence {
                    snapshot_id: read_only_fence.snapshot.id.clone(),
                    runtime_incarnation: Some(read_only_attachment.runtime_incarnation.clone()),
                    terminal_sequence: Some(read_only_fence.snapshot.store_index),
                    ..Fence::default()
                },
                TerminalResizeParameters {
                    terminal_id: read_only_attachment.terminal_id.clone(),
                    rows: 40,
                    columns: 120,
                },
            )
            .await,
    ] {
        assert!(
            matches!(denied, Err(ClientError::Api(ErrorCode::Forbidden, _, _))),
            "terminal input and resize require terminal.control"
        );
    }
    detach_terminal(&read_only, &read_only_attachment, "unix-read-only").await;

    let first_attachment = attach_terminal(&client, "unix-first").await;
    let (first_stream, first) =
        first_screen(&client, &first_attachment, "terminal-demo-runtime:i1")
            .await
            .unwrap();
    assert_eq!(first.value.runtime_incarnation, "terminal-demo-runtime:i1");
    assert_eq!(first.value.lines[0].text, "terminal ready");
    assert_eq!(first.value.lines[1].text, "$");
    assert_eq!((first.value.rows, first.value.columns), (24, 80));
    assert_eq!((first.value.cursor.row, first.value.cursor.column), (1, 2));
    first_stream.close().await;

    let control_fence = client.capabilities().await.unwrap();
    let input = client
        .terminal_input(
            "action/person-terminal-input",
            "person-terminal-input-0001",
            Fence {
                snapshot_id: control_fence.snapshot.id.clone(),
                runtime_incarnation: Some(first_attachment.runtime_incarnation.clone()),
                terminal_sequence: Some(control_fence.snapshot.store_index),
                ..Fence::default()
            },
            TerminalInputParameters {
                terminal_id: first_attachment.terminal_id.clone(),
                mode: TerminalInputMode::Line,
                value: "actor attribution proof".into(),
            },
        )
        .await;
    assert!(
        input.is_err(),
        "the fake PTY cannot accept input, but the durable request/result path must run"
    );
    let control_claims = state
        .store
        .claims_for("agent/terminal-demo", None)
        .unwrap()
        .into_iter()
        .filter(|claim| {
            matches!(
                claim.kind.as_str(),
                "terminal.input.requested" | "terminal.input.result"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(control_claims.len(), 2);
    assert!(
        control_claims
            .iter()
            .all(|claim| claim.actor.as_deref() == Some("person/ada")),
        "terminal control attribution must come from the authenticated session: {control_claims:?}"
    );

    assert!(
        client
            .terminal_stream(
                &first_attachment.terminal_id,
                Some("terminal-demo-runtime:wrong"),
                attach_terminal(&client, "unix-wrong")
                    .await
                    .stream_capability
                    .as_deref()
                    .unwrap(),
            )
            .await
            .is_err(),
        "a stale incarnation must be rejected during the WebSocket handshake"
    );

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let mut wrong_protocol = "ws://localhost/v1/client/terminals/agent/terminal-demo/stream"
        .into_client_request()
        .unwrap();
    wrong_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        "wrong.terminal.protocol".parse().unwrap(),
    );
    assert!(
        tokio_tungstenite::client_async(wrong_protocol, stream)
            .await
            .is_err(),
        "the Unix WebSocket must reject the wrong subprotocol"
    );

    let replacement_fence = attach_terminal(&client, "unix-replaced").await;
    publish_terminal(&state, "terminal-demo-runtime:i2");
    assert!(
        client
            .terminal_stream(
                &replacement_fence.terminal_id,
                Some("terminal-demo-runtime:i1"),
                replacement_fence.stream_capability.as_deref().unwrap(),
            )
            .await
            .is_err(),
        "reconnect to a replaced incarnation must reject the old fence"
    );
    let second_attachment = attach_terminal(&client, "unix-second").await;
    let (_replacement_stream, replacement) =
        first_screen(&client, &second_attachment, "terminal-demo-runtime:i2")
            .await
            .unwrap();
    assert_eq!(
        replacement.value.runtime_incarnation,
        "terminal-demo-runtime:i2"
    );
    assert!(
        client
            .terminal_stream(
                &second_attachment.terminal_id,
                Some("terminal-demo-runtime:i2"),
                second_attachment.stream_capability.as_deref().unwrap(),
            )
            .await
            .is_err(),
        "a stream capability must be single use"
    );
    let detached = attach_terminal(&client, "unix-detach").await;
    detach_terminal(&client, &detached, "unix-first").await;
    detach_terminal(&client, &detached, "unix-repeat").await;
    assert!(
        client
            .terminal_stream(
                &detached.terminal_id,
                Some("terminal-demo-runtime:i2"),
                detached.stream_capability.as_deref().unwrap(),
            )
            .await
            .is_err(),
        "an idempotently detached viewer must not reconnect"
    );
    server.abort();
}

#[tokio::test]
async fn generated_client_conforms_over_paired_loopback_and_rejects_bad_credentials() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path(), "client-loopback");
    publish_terminal(&state, "terminal-demo-runtime:fabric-i1");
    let _pty = FakePty::start(&state, 24, 80);

    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let unix_app = st3::api::router(state.clone());
    let unix_server =
        tokio::spawn(async move { st3::api::serve_unix(&server_socket, unix_app).await });
    wait_for_socket(&socket).await;
    let local = Client::unix_as(&socket, "person/ada");
    let challenge = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Conformance phone".into(),
            person_id: "person/ada".into(),
            full_control: None,
        })
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = st3::api::fabric_router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{address}");
    let paired = Client::fabric_pairing(&base)
        .pairing_complete(
            &challenge.value.pairing_id,
            &PairingComplete {
                api_version: st3_client::API_VERSION.into(),
                code: challenge.value.code,
                device_public_key: "conformance-public-key-000000000000000000000000".into(),
            },
        )
        .await
        .unwrap();

    let gateway_socket = root.path().join("st3-client.sock");
    let server_gateway_socket = gateway_socket.clone();
    let gateway_app = st3::api::fabric_router(state.clone());
    let gateway_server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_gateway_socket, gateway_app).await },
        );
    wait_for_socket(&gateway_socket).await;
    assert_private_socket(&gateway_socket);
    assert_eq!(
        unix_status(&gateway_socket, "/v1/health", None).await,
        StatusCode::OK
    );
    assert_eq!(
        unix_status(&gateway_socket, "/v1/client/capabilities", None).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        Client::unix(&gateway_socket).capabilities().await.is_err(),
        "the paired-only Unix gateway must reject an ordinary local session"
    );
    let direct_gateway = Client::unix_gateway(&gateway_socket, &paired.value.credential);
    let direct_capabilities = direct_gateway.capabilities().await.unwrap();
    assert_eq!(
        direct_capabilities.value.transport,
        st3_client::TransportKind::FabricLoopback
    );
    assert_eq!(
        unix_status(
            &gateway_socket,
            "/v1/schema",
            Some(&paired.value.credential)
        )
        .await,
        StatusCode::FORBIDDEN,
        "the client gateway must never expose privileged non-client routes"
    );
    let direct_attachment = attach_terminal(&direct_gateway, "gateway-unix").await;
    direct_gateway
        .terminal_stream(
            &direct_attachment.terminal_id,
            Some("terminal-demo-runtime:fabric-i1"),
            direct_attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();

    let http = reqwest::Client::new();

    let client = Client::fabric_loopback(&base, &paired.value.credential);
    let capabilities: Envelope<Capabilities> = client.capabilities().await.unwrap();
    assert_eq!(
        capabilities.value.transport,
        st3_client::TransportKind::FabricLoopback
    );
    assert_eq!(capabilities.value.session_actor, paired.value.session_actor);
    assert_eq!(paired.value.person_id, "person/ada");
    assert_eq!(
        client
            .operations_list(None, None, false)
            .await
            .unwrap()
            .value
            .collection,
        "operations"
    );

    let denied = http
        .get(format!("{base}/v1/client/capabilities"))
        .bearer_auth("not-a-real-client-credential")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let error: st3_client::ErrorEnvelope = denied.json().await.unwrap();
    assert_eq!(error.code, st3_client::ErrorCode::Forbidden);

    // A phone's one socket: the paired gateway passes the collections subprotocol, holds a
    // window, and carries a terminal on the same socket. A bad credential never opens it.
    assert!(
        Client::fabric_loopback(&base, "not-a-real-client-credential")
            .collection_stream()
            .await
            .is_err()
    );
    let mut collections = client.collection_stream().await.unwrap();
    collections
        .subscribe("missions", "missions", 20, None, None)
        .await
        .unwrap();
    assert!(matches!(
        next_collection_event(&mut collections, "the paired missions snapshot").await,
        CollectionEvent::Snapshot { id, .. } if id == "missions"
    ));
    let socket_attachment = attach_terminal(&client, "fabric-collection").await;
    collections
        .subscribe_terminal(
            "screen",
            &socket_attachment.terminal_id,
            Some("terminal-demo-runtime:fabric-i1"),
            socket_attachment.stream_capability.as_deref().unwrap(),
        )
        .await
        .unwrap();
    loop {
        match next_collection_event(&mut collections, "the paired terminal screen").await {
            CollectionEvent::Screen { id, screen } if id == "screen" => {
                assert_eq!(
                    screen.value.runtime_incarnation,
                    "terminal-demo-runtime:fabric-i1"
                );
                break;
            }
            CollectionEvent::Changes { id, .. } if id == "missions" => {}
            other => panic!("expected the paired terminal screen, got {other:?}"),
        }
    }
    drop(collections);

    let fabric_attachment = attach_terminal(&client, "fabric-first").await;
    let (_terminal_stream, terminal) =
        first_screen(&client, &fabric_attachment, "terminal-demo-runtime:fabric-i1")
            .await
            .unwrap();
    assert_eq!(
        terminal.value.runtime_incarnation,
        "terminal-demo-runtime:fabric-i1"
    );
    assert_eq!(terminal.value.lines[0].text, "terminal ready");
    let bad_attachment = "not-a-real-stream-capability-0000000000";
    assert!(
        Client::fabric_loopback(&base, "not-a-real-client-credential")
            .terminal_stream(
                &fabric_attachment.terminal_id,
                Some("terminal-demo-runtime:fabric-i1"),
                bad_attachment,
            )
            .await
            .is_err(),
        "the Fabric WebSocket must authenticate before upgrade"
    );

    let mut wrong_protocol = format!(
        "ws://{address}/v1/client/terminals/agent%2Fterminal-demo/stream?incarnation=terminal-demo-runtime%3Afabric-i1"
    )
    .into_client_request()
    .unwrap();
    wrong_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        "wrong.terminal.protocol".parse().unwrap(),
    );
    wrong_protocol.headers_mut().insert(
        "authorization",
        format!("Bearer {}", paired.value.credential)
            .parse()
            .unwrap(),
    );
    assert!(
        tokio_tungstenite::connect_async(wrong_protocol)
            .await
            .is_err(),
        "the authenticated Fabric WebSocket must reject the wrong subprotocol"
    );

    let revoked_attachment = attach_terminal(&client, "fabric-revoked").await;
    let local_capabilities = local.capabilities().await.unwrap();
    local
        .pairing_revoke(
            "action/pairing-revoke-fabric",
            "pairing-revoke-fabric-000001",
            Fence {
                snapshot_id: local_capabilities.snapshot.id,
                ..Fence::default()
            },
            TargetParameters {
                target_id: paired.value.device_id.clone(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap();
    assert!(
        client
            .terminal_stream(
                &revoked_attachment.terminal_id,
                Some("terminal-demo-runtime:fabric-i1"),
                revoked_attachment.stream_capability.as_deref().unwrap(),
            )
            .await
            .is_err(),
        "credential revocation must invalidate an unused stream capability"
    );
    assert!(
        direct_gateway.capabilities().await.is_err(),
        "the revoked bearer must also fail on the paired-only Unix gateway"
    );
    local
        .capabilities()
        .await
        .expect("the privileged local socket remains independent");

    server.abort();
    gateway_server.abort();
    unix_server.abort();
    let _ = server.await;
    let _ = gateway_server.await;
    let _ = unix_server.await;

    let restarted_local_socket = socket.clone();
    let restarted_local_app = st3::api::router(state.clone());
    let restarted_local = tokio::spawn(async move {
        st3::api::serve_unix(&restarted_local_socket, restarted_local_app).await
    });
    let restarted_gateway_socket = gateway_socket.clone();
    let restarted_gateway_app = st3::api::fabric_router(state);
    let restarted_gateway = tokio::spawn(async move {
        st3::api::serve_unix(&restarted_gateway_socket, restarted_gateway_app).await
    });
    wait_for_socket(&socket).await;
    wait_for_socket(&gateway_socket).await;
    assert_private_socket(&socket);
    assert_private_socket(&gateway_socket);
    Client::unix_as(&socket, "person/ada")
        .capabilities()
        .await
        .expect("the privileged local socket restarts independently");
    assert_eq!(
        unix_status(&gateway_socket, "/v1/health", None).await,
        StatusCode::OK
    );
    assert_eq!(
        unix_status(&gateway_socket, "/v1/client/capabilities", None).await,
        StatusCode::FORBIDDEN
    );
    restarted_gateway.abort();
    restarted_local.abort();
}
