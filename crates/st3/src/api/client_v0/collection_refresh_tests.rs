//! Which windows a collection socket reads again, and when: only those whose inputs changed.
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Reads = Arc<Mutex<BTreeMap<String, usize>>>;

struct Fixture {
    state: AppState,
    reads: Reads,
    socket: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    /// A socket whose reads are counted. `real` reads the store's own windows; otherwise each
    /// read returns one row naming how many reads that subscription has had, and the
    /// subscription named `fail` fails its first read.
    async fn open(state: AppState, real: bool, fail: Option<&'static str>, collections: &[&str]) -> Self {
        let reads = Reads::default();
        let (serving, counted) = (state.clone(), reads.clone());
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, reads) = (serving.clone(), counted.clone());
                async move {
                    upgrade.on_upgrade(move |socket| {
                        let windows = collection_windows::Windows::attach(&state.store);
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            ClientSession::local(None).unwrap(),
                            None,
                            move |state, session, request, permit| {
                                let (reads, windows) = (reads.clone(), windows.clone());
                                async move {
                                    let count = {
                                        let mut reads = reads.lock().unwrap();
                                        let count = reads.entry(request.id.clone()).or_default();
                                        *count += 1;
                                        *count
                                    };
                                    if real {
                                        return collection_items_with_windows(
                                            &state, &session, &request, permit, windows,
                                        )
                                        .await;
                                    }
                                    let _permit = permit;
                                    if fail == Some(request.id.as_str()) && count == 1 {
                                        return Err(ApiError::internal("injected first read failure"));
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
        let mut fixture = Self { state, reads, socket, server };
        for collection in collections {
            fixture.subscribe(collection, collection).await;
        }
        fixture
    }

    async fn subscribe(&mut self, id: &str, collection: &str) {
        self.socket
            .send(Message::Text(
                json!({"kind":"subscribe","id":id,"collection":collection,"limit":200})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }

    async fn frame(&mut self) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
            .await
            .expect("a collection frame")
            .unwrap()
            .unwrap();
        serde_json::from_str(message.to_text().unwrap()).unwrap()
    }

    /// No frame arrives for longer than a paced reread can take.
    async fn quiet(&mut self) {
        let waited = tokio::time::timeout(
            COLLECTION_REREAD_INTERVAL + Duration::from_millis(500),
            self.socket.next(),
        )
        .await;
        assert!(waited.is_err(), "unexpected frame: {waited:?}");
    }

    fn counts(&self) -> BTreeMap<String, usize> {
        self.reads.lock().unwrap().clone()
    }

    fn claim(&self, subject: &str, kind: &str, fields: Value) {
        self.state
            .store
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
        signal_changed(&self.state);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(id, count)| ((*id).to_owned(), *count)).collect()
}

#[tokio::test]
async fn a_retry_reads_only_the_failed_subscription() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    let mut fixture = Fixture::open(state, false, Some("agents"), &["missions", "agents"]).await;
    let mut initial = BTreeMap::new();
    for _ in 0..2 {
        let frame = fixture.frame().await;
        initial.insert(frame["id"].as_str().unwrap().to_owned(), frame);
    }
    assert_eq!(initial["missions"]["kind"], "snapshot");
    assert_eq!(initial["agents"]["kind"], "resync");
    let recovered = fixture.frame().await;
    assert_eq!((recovered["id"].as_str(), recovered["kind"].as_str()), (Some("agents"), Some("snapshot")));
    assert_eq!(fixture.counts(), counts(&[("agents", 2), ("missions", 1)]));
}

#[tokio::test]
async fn a_usage_commit_rereads_agents_and_leaves_missions_idle() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    let mut fixture = Fixture::open(state, false, None, &["missions", "agents"]).await;
    for _ in 0..2 {
        assert_eq!(fixture.frame().await["kind"], "snapshot");
    }
    // Establish the socket's revision baseline with a commit both windows show.
    fixture.claim("agent/fixture-roster", "runtime.observed", json!({"status":"running"}));
    for _ in 0..2 {
        assert_eq!(fixture.frame().await["kind"], "changes");
    }
    fixture.claim("agent/fixture-roster", "harness.usage", json!({"driver":"codex",
        "semantics":"response", "incarnation_id":"fixture-one", "input_tokens":12}));
    let changed = fixture.frame().await;
    assert_eq!((changed["id"].as_str(), changed["kind"].as_str()), (Some("agents"), Some("changes")));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("agents", 3), ("missions", 2)]));
    // A commit no window shows reads nothing and sends nothing.
    fixture.claim("daemon/fixture", "daemon.diagnostic",
        json!({"code":"fixture", "severity":"error", "reason":"unrelated"}));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("agents", 3), ("missions", 2)]));
}

#[tokio::test]
async fn a_published_view_rereads_when_it_publishes_and_not_on_commits() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    let mut fixture = Fixture::open(state.clone(), false, None, &["missions", "work"]).await;
    for _ in 0..2 {
        assert_eq!(fixture.frame().await["kind"], "snapshot");
    }
    // A refresher now keeps work published: its window rereads that publication alone.
    state.store.publish_collection_view("work");
    let published = fixture.frame().await;
    assert_eq!((published["id"].as_str(), published["kind"].as_str()), (Some("work"), Some("changes")));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("missions", 1), ("work", 2)]));
    // A commit both collections show rereads missions, which still follows commits, and not
    // work, whose refresher publishes it.
    fixture.claim("agent/fixture-roster", "runtime.observed", json!({"status":"running"}));
    let changed = fixture.frame().await;
    assert_eq!((changed["id"].as_str(), changed["kind"].as_str()), (Some("missions"), Some("changes")));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("missions", 2), ("work", 2)]));
    // Each later publication rereads work once more.
    state.store.publish_collection_view("work");
    assert_eq!(fixture.frame().await["id"], "work");
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("missions", 2), ("work", 3)]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agents_commit_waits_for_the_roster_publication_instead_of_rereading() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state(root.path());
    let subject = "agent/routed-roster";
    let append = |kind: &str, fields: Value| {
        state.store.append_claim(&ClaimInput {
            subject: subject.into(), kind: kind.into(), actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        signal_changed(&state);
    };
    append("runtime.observed", json!({"status":"running",
        "runtime_id":"routed-roster", "incarnation_id":"one"}));
    let mut published = state.store.subscribe_agent_roster();
    crate::api::start_agent_roster(&state);
    tokio::time::timeout(Duration::from_secs(5), published.wait_for(|revision| *revision > 0))
        .await.expect("the daemon publishes its roster as it starts").unwrap();
    let mut fixture = Fixture::open(state.clone(), true, None, &["agents"]).await;
    let snapshot = fixture.frame().await;
    assert_eq!(snapshot["kind"], "snapshot", "{snapshot}");
    assert!(snapshot["items"][0]["harness_state"].is_null(), "{snapshot}");
    // Let any publication the first read asked for settle, then hold the next refresh back.
    tokio::time::sleep(COLLECTION_REREAD_INTERVAL).await;
    while tokio::time::timeout(Duration::from_millis(200), fixture.socket.next()).await.is_ok() {}
    let held = state.store.admit_agent_resources().await;
    let before = fixture.counts()["agents"];
    append("harness.observed", json!({"state":"working", "driver":"codex", "incarnation_id":"one"}));
    // Reading now would only serve the roster already held.
    fixture.quiet().await;
    assert_eq!(fixture.counts()["agents"], before, "no read before the roster is published");
    drop(held);
    let changes = fixture.frame().await;
    assert_eq!(changes["kind"], "changes", "{changes}");
    assert_eq!(changes["upserts"][0]["harness_state"], "working", "{changes}");
    assert_eq!(fixture.counts()["agents"], before + 1, "one read, of the newer roster");
}
