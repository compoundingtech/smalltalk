use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

struct Fixture {
    state: AppState,
    reads: Arc<Mutex<BTreeMap<String, usize>>>,
    socket: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn open(root: &Path, fail_first: bool) -> Self {
        let state = super::tests::test_state_named(root, "alder");
        let reads = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
        let (serving, counted) = (state.clone(), reads.clone());
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, reads) = (serving.clone(), counted.clone());
                async move {
                    upgrade.on_upgrade(move |socket| {
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            ClientSession::local(None).unwrap(),
                            None,
                            move |state, _, request, permit| {
                                let reads = reads.clone();
                                async move {
                                    let _permit = permit;
                                    let count = {
                                        let mut reads = reads.lock().unwrap();
                                        let count = reads.entry(request.id.clone()).or_default();
                                        *count += 1;
                                        *count
                                    };
                                    if fail_first && request.id == "agents" && count == 1 {
                                        return Err(ApiError::internal(
                                            "injected first read failure",
                                        ));
                                    }
                                    Ok((
                                        new_client_snapshot(&state),
                                        vec![json!({"id":request.id,"read":count})],
                                        false,
                                    ))
                                }
                            },
                        )
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
            .await
            .unwrap();
        let mut fixture = Self {
            state,
            reads,
            socket,
            server,
        };
        for collection in ["missions", "agents"] {
            fixture
                .socket
                .send(Message::Text(
                    json!({
                        "kind":"subscribe","id":collection,"collection":collection,"limit":2
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
        fixture
    }

    async fn frame(&mut self) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(5), self.socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    fn counts(&self) -> BTreeMap<String, usize> {
        self.reads.lock().unwrap().clone()
    }

    fn claim(&self, kind: &str) {
        self.state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/fixture-roster".into(),
                kind: kind.into(),
                actor: None,
                fields: if kind == "harness.usage" {
                    BTreeMap::from([
                        ("driver".into(), json!("codex")),
                        ("semantics".into(), json!("response")),
                        ("incarnation_id".into(), json!("fixture-one")),
                        ("input_tokens".into(), json!(12)),
                    ])
                } else {
                    BTreeMap::from([("status".into(), json!("running"))])
                },
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        signal_changed(&self.state);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn retry_reads_only_the_failed_subscription() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::open(root.path(), true).await;
    let mut initial = BTreeMap::new();
    for _ in 0..2 {
        let frame = fixture.frame().await;
        initial.insert(frame["id"].as_str().unwrap().to_owned(), frame);
    }
    assert_eq!(initial["missions"]["kind"], "snapshot");
    assert_eq!(initial["agents"]["kind"], "resync");
    let recovered = fixture.frame().await;
    assert_eq!(recovered["id"], "agents");
    assert_eq!(recovered["kind"], "snapshot");
    assert_eq!(
        fixture.counts(),
        BTreeMap::from([("agents".into(), 2), ("missions".into(), 1)])
    );
}

#[tokio::test]
async fn an_agent_usage_commit_leaves_the_mission_window_idle() {
    let root = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::open(root.path(), false).await;
    for _ in 0..2 {
        assert_eq!(fixture.frame().await["kind"], "snapshot");
    }
    // Establish the socket's shared revision baseline before the targeted mutation.
    fixture.claim("runtime.observed");
    for _ in 0..2 {
        assert_eq!(fixture.frame().await["kind"], "changes");
    }
    fixture.claim("harness.usage");
    let changed = fixture.frame().await;
    assert_eq!(changed["id"], "agents");
    assert_eq!(changed["kind"], "changes");
    assert!(
        tokio::time::timeout(
            COLLECTION_REREAD_INTERVAL + Duration::from_millis(200),
            fixture.socket.next()
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture.counts(),
        BTreeMap::from([("agents".into(), 3), ("missions".into(), 2)])
    );
}
