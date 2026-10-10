//! Which windows a collection socket reads again, and when: only those whose inputs changed.
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Reads = Arc<Mutex<BTreeMap<String, usize>>>;

/// [`Fixture::open_with`]'s gate: reads refuse as not ready, read rows, or refuse as revoked.
const NOT_READY: u8 = 0;
const READY: u8 = 1;
const REVOKED: u8 = 2;

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
        Self::open_as(state, ClientSession::local(None).unwrap(), real, fail, collections).await
    }

    async fn open_as(
        state: AppState,
        session: ClientSession,
        real: bool,
        fail: Option<&'static str>,
        collections: &[&str],
    ) -> Self {
        Self::open_with(state, session, real, fail, None, collections).await
    }

    /// As [`Self::open_as`]; with `gate`, each counted read is the work list's not-ready refusal
    /// while it is [`NOT_READY`], and a revoked grant's refusal once it is [`REVOKED`].
    async fn open_with(
        state: AppState,
        session: ClientSession,
        real: bool,
        fail: Option<&'static str>,
        gate: Option<Arc<std::sync::atomic::AtomicU8>>,
        collections: &[&str],
    ) -> Self {
        let reads = Reads::default();
        let (serving, counted) = (state.clone(), reads.clone());
        let app = axum::Router::new().route(
            "/stream",
            axum::routing::get(move |upgrade: WebSocketUpgrade| {
                let (state, reads, session) = (serving.clone(), counted.clone(), session.clone());
                let gate = gate.clone();
                async move {
                    upgrade.on_upgrade(move |socket| {
                        let windows = collection_windows::Windows::attach(&state.store);
                        collection_stream_socket_with_reader(
                            socket,
                            state,
                            session,
                            None,
                            move |state, session, request, permit| {
                                let (reads, windows) = (reads.clone(), windows.clone());
                                let gate = gate.clone();
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
                                    match gate.as_ref().map(|gate| gate.load(std::sync::atomic::Ordering::SeqCst)) {
                                        Some(NOT_READY) => {
                                            return Err(super::super::published_lists::work_list_not_ready());
                                        }
                                        Some(REVOKED) => {
                                            return Err(ApiError {
                                                status: StatusCode::FORBIDDEN,
                                                code: "forbidden".into(),
                                                message: "the pairing grant was revoked".into(),
                                                details: Box::default(),
                                            });
                                        }
                                        _ => {}
                                    }
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
        self.send(json!({"kind":"subscribe","id":id,"collection":collection,"limit":200})).await;
    }

    async fn send(&mut self, command: Value) {
        self.socket.send(Message::Text(command.to_string().into())).await.unwrap();
    }

    async fn frame(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.socket.next())
                .await
                .expect("a collection frame")
                .unwrap()
                .unwrap();
            // Protocol pings carry no frame.
            if let Message::Text(text) = message {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    /// No frame arrives for longer than a paced reread can take.
    async fn quiet(&mut self) {
        let until = tokio::time::Instant::now() + COLLECTION_REREAD_INTERVAL + Duration::from_millis(500);
        while let Ok(message) = tokio::time::timeout_at(until, self.socket.next()).await {
            if let Message::Text(text) = message.unwrap().unwrap() {
                panic!("unexpected frame: {text}");
            }
        }
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
    // A withdrawn view is read once more, then follows commits like any other window.
    state.store.withdraw_collection_view("work");
    assert_eq!(fixture.frame().await["id"], "work");
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("missions", 2), ("work", 4)]));
    fixture.claim("agent/fixture-roster", "runtime.observed", json!({"status":"stopped"}));
    let mut changed = BTreeSet::new();
    for _ in 0..2 {
        changed.insert(fixture.frame().await["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(changed, BTreeSet::from(["missions".to_owned(), "work".to_owned()]));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("missions", 3), ("work", 5)]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agents_commit_waits_for_the_roster_publication_instead_of_rereading() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "routed-roster");
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

/// What a client holds for one window after applying every frame in order.
#[derive(Default, Clone, PartialEq, Debug)]
struct Held {
    rows: BTreeMap<String, Value>,
    order: Vec<String>,
    has_more: bool,
}

impl Held {
    fn items(&self) -> Vec<Value> {
        self.order.iter().map(|id| self.rows[id].clone()).collect()
    }
}

/// Apply one frame as a client does: a snapshot replaces the window; changes apply removals,
/// then upserts, then the new order. The rows held are always exactly the ordered ones.
fn apply(held: &mut BTreeMap<String, Held>, frame: &Value) {
    let id = frame["id"].as_str().unwrap().to_owned();
    let window = held.entry(id).or_default();
    let ids = |value: &Value| value.as_array().unwrap().iter()
        .map(|id| id.as_str().unwrap().to_owned()).collect::<Vec<_>>();
    match frame["kind"].as_str().unwrap() {
        "snapshot" => {
            window.rows = frame["items"].as_array().unwrap().iter()
                .map(|item| (item["id"].as_str().unwrap().to_owned(), item.clone())).collect();
        }
        "changes" => {
            for removed in ids(&frame["removes"]) {
                window.rows.remove(&removed);
            }
            for item in frame["upserts"].as_array().unwrap() {
                window.rows.insert(item["id"].as_str().unwrap().to_owned(), item.clone());
            }
        }
        // A temporary read failure keeps the rows the client holds; the window is read again.
        "resync" => return,
        other => panic!("unexpected {other} frame: {frame}"),
    }
    window.order = ids(&frame["order"]);
    window.has_more = frame["has_more"].as_bool().unwrap();
    assert_eq!(
        window.rows.keys().cloned().collect::<BTreeSet<_>>(),
        window.order.iter().cloned().collect::<BTreeSet<_>>(),
        "the rows a client holds are the window's order: {frame}"
    );
}

/// Apply frames until the socket is quiet and the agents roster is published at the newest
/// cut, so every held window has caught up with the store.
async fn settle(fixture: &mut Fixture, held: &mut BTreeMap<String, Held>) {
    loop {
        match tokio::time::timeout(COLLECTION_REREAD_INTERVAL + Duration::from_millis(700),
            fixture.socket.next()).await
        {
            Ok(message) => {
                // Protocol pings carry no frame.
                if let Message::Text(text) = message.unwrap().unwrap() {
                    apply(held, &serde_json::from_str(&text).unwrap());
                }
            }
            Err(_) => {
                let index = fixture.state.store.index().unwrap();
                if fixture.state.store.agent_roster_current(index, false).unwrap() {
                    return;
                }
                fixture.state.store.request_agent_roster_refresh();
            }
        }
    }
}

/// Every held window is what a full read of its subscription gives now.
async fn assert_parity(fixture: &Fixture, session: &ClientSession, held: &BTreeMap<String, Held>,
    requests: &[Value], step: &str)
{
    for request in requests {
        let request: CollectionSubscribe = serde_json::from_value(request.clone()).unwrap();
        let permit = Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap();
        let (_, items, has_more) = collection_items(&fixture.state, session, &request, permit)
            .await.unwrap();
        let window = &held[&request.id];
        assert_eq!(window.items(), items, "{step}: window {} differs from a full read", request.id);
        assert_eq!(window.has_more, has_more, "{step}: window {}", request.id);
        // A published work window is also what the direct read it replaces gives at its cut.
        if request.collection == "work" {
            let store = &fixture.state.store;
            let index = store.index().unwrap();
            let published = store.published_work().expect("the work list is published");
            assert_eq!(published.cut, index, "{step}: the work list is published at the current cut");
            let mut direct = client_work_resources(
                store, request.actor.as_deref(), false, store.projection_time_at(index).unwrap(), index,
            ).unwrap();
            direct.truncate(request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS));
            assert_eq!(window.items(), direct, "{step}: work window {} differs from a direct read", request.id);
        }
    }
}

fn custom_review_kind(store: &Store) {
    let manifest = serde_json::from_str(include_str!(
        "../../../../../examples/st3/custom-review.json"
    )).unwrap();
    store.register_custom_kind(&crate::store::custom::RegistrationRequest {
        manifest,
        actor: "agent/garden/seed".into(),
    }).unwrap();
}

fn claim(store: &Store, subject: &str, kind: &str, actor: Option<&str>, fields: Value) {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: kind.into(), actor: actor.map(str::to_owned),
        fields: serde_json::from_value(fields).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
}

/// Seats, a mission run with a step and review requests for two people.
fn orchard(store: &Store) {
    custom_review_kind(store);
    for (subject, incarnation) in [("agent/alder.plain", "plain-1"), ("agent/alder.quiet", "quiet-1")] {
        claim(store, subject, "runtime.observed", Some(subject),
            json!({"status":"running", "runtime_id":subject, "incarnation_id":incarnation}));
    }
    claim(store, "agent/alder.plain", "harness.observed", Some("agent/alder.plain"),
        json!({"state":"working", "driver":"omp", "incarnation_id":"plain-1"}));
    let source = r#"version 2
mission "orchard-crew" state="ready" {
  goal "Keep the orchard's windows honest."
  step "prune" { agentless; goal "Prune." }
}"#;
    let intent = crate::graph::parse_intent(source, "alder").unwrap();
    let planned = store.mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "orchard-crew").unwrap();
    store.create_mission_run(&crate::model::MissionRunRequest {
        mission: "orchard-crew".into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/avery".into()), mode: Some("run".into()),
        inputs: BTreeMap::new(), idempotency_key: "orchard-crew-run".into(),
    }).unwrap();
    for (subject, person) in [("custom/garden/review/v1/orchard-avery", "person/avery"), ("custom/garden/review/v1/orchard-robin", "person/robin")] {
        claim(store, subject, "custom.garden.review.v1.requested", Some("agent/garden/seed"),
            json!({"title":"Choose a seed", "detail":"Keep or discard", "recipient":person}));
    }
}

/// A client's windows equal full reads through replication in any order, a rolled-back write,
/// a checkpoint trim, a store reopen with a reconnect, and each person's own visibility.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clients_hold_what_full_reads_give_through_replication_rollback_checkpoint_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    orchard(&state.store);
    crate::api::start_agent_roster(&state);
    // Work windows serve the published list; each is also checked against a direct read.
    super::super::published_lists::start_published_work(&state);
    let requests = [
        json!({"kind":"subscribe","id":"agents","collection":"agents","limit":200}),
        json!({"kind":"subscribe","id":"missions","collection":"missions","limit":200}),
        json!({"kind":"subscribe","id":"work","collection":"work","limit":200}),
        json!({"kind":"subscribe","id":"plain-work","collection":"work","actor":"agent/alder.plain","limit":200}),
        json!({"kind":"subscribe","id":"avery","collection":"attention","person":"person/avery","limit":200}),
        json!({"kind":"subscribe","id":"robin","collection":"attention","person":"person/robin","limit":200}),
        json!({"kind":"subscribe","id":"agents-2","collection":"agents","limit":2}),
    ];
    let session = ClientSession::local(None).unwrap();
    let mut fixture = Fixture::open(state.clone(), true, None, &[]).await;
    for request in &requests {
        fixture.send(request.clone()).await;
    }
    let mut held = BTreeMap::new();
    settle(&mut fixture, &mut held).await;
    assert_parity(&fixture, &session, &held, &requests, "seeded").await;
    assert!(held["avery"].order.iter().all(|id| !held["robin"].order.contains(id)));
    assert!(!held["avery"].order.is_empty() && !held["robin"].order.is_empty());

    // Another host's claims, delivered in reverse envelope order and then again.
    let birch = Store::open_memory("birch").unwrap();
    claim(&birch, "agent/birch.solo", "runtime.observed", Some("agent/birch.solo"),
        json!({"status":"running", "runtime_id":"agent/birch.solo", "incarnation_id":"solo-1"}));
    claim(&birch, "agent/birch.solo", "harness.observed", Some("agent/birch.solo"),
        json!({"state":"idle", "driver":"omp", "incarnation_id":"solo-1"}));
    claim(&birch, "agent/alder.plain", "runtime.observed", Some("agent/alder.plain"),
        json!({"status":"running", "runtime_id":"agent/alder.plain", "incarnation_id":"plain-birch"}));
    for store in [&birch, &*state.store] {
        store.bind_fleet("fleet/orchard").unwrap();
    }
    let mut exchange = birch
        .export_replication_exchange("fleet/orchard", &smallclaims::replication::ReplicationInventory::default())
        .unwrap();
    assert!(exchange.envelopes.len() >= 3, "a meaningful envelope permutation");
    exchange.envelopes.reverse();
    for _ in 0..2 {
        state.store.receive_replication_exchange("birch", "fleet/orchard", &exchange).unwrap();
        state.store.validate_replication_backlog().unwrap();
        state.store.apply_replication_repairs().unwrap();
        state.store.project_replication_backlog().unwrap();
        signal_changed(&state);
        settle(&mut fixture, &mut held).await;
        assert_parity(&fixture, &session, &held, &requests, "replicated").await;
    }
    assert!(held["agents"].order.iter().any(|id| id == "agent/birch.solo"));

    // A write that rolls back changes nothing a client holds.
    let before = held.clone();
    state.store.roll_back_claim_for_test(&ClaimInput {
        subject: "agent/alder.quiet".into(), kind: "runtime.observed".into(),
        actor: Some("agent/alder.quiet".into()),
        fields: serde_json::from_value(json!({"status":"stopped",
            "runtime_id":"agent/alder.quiet", "incarnation_id":"quiet-1"})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    });
    signal_changed(&state);
    settle(&mut fixture, &mut held).await;
    assert_eq!(held, before, "a rolled-back write sends nothing");
    assert_parity(&fixture, &session, &held, &requests, "rolled back").await;

    // Ordinary changes, then a checkpoint that trims the history behind them.
    claim(&state.store, "agent/alder.plain", "harness.observed", Some("agent/alder.plain"),
        json!({"state":"idle", "driver":"omp", "incarnation_id":"plain-1"}));
    claim(&state.store, "agent/alder.quiet", "runtime.observed", Some("agent/alder.quiet"),
        json!({"status":"stopped", "runtime_id":"agent/alder.quiet", "incarnation_id":"quiet-1"}));
    signal_changed(&state);
    settle(&mut fixture, &mut held).await;
    assert_parity(&fixture, &session, &held, &requests, "changed").await;
    // History a checkpoint drops: superseded harness states and old diagnostics.
    for (n, harness) in ["working", "idle", "working", "idle"].into_iter().enumerate() {
        claim(&state.store, "agent/alder.plain", "harness.observed", Some("agent/alder.plain"),
            json!({"state":harness, "driver":"omp", "incarnation_id":"plain-1", "observed_at_ms":n}));
    }
    for n in 0..4 {
        claim(&state.store, "daemon/alder", "daemon.diagnostic", None,
            json!({"severity":"warning", "code":"slow-request", "reason":format!("slow {n}")}));
    }
    signal_changed(&state);
    settle(&mut fixture, &mut held).await;
    assert_parity(&fixture, &session, &held, &requests, "before the checkpoint").await;
    assert!(state.store.trim_checkpoint_for_test(client_now_ms() + 1_000) > 0, "the checkpoint drops history");
    state.store.forget_current_views();
    signal_changed(&state);
    settle(&mut fixture, &mut held).await;
    assert_parity(&fixture, &session, &held, &requests, "checkpointed").await;

    // Reopen the store and reconnect: the new snapshots are the windows the client held.
    let before = held.clone();
    drop(fixture);
    let reopened = super::tests::test_state_named(root.path(), "alder");
    crate::api::start_agent_roster(&reopened);
    super::super::published_lists::start_published_work(&reopened);
    let mut fixture = Fixture::open(reopened.clone(), true, None, &[]).await;
    for request in &requests {
        fixture.send(request.clone()).await;
    }
    let mut reconnected = BTreeMap::new();
    settle(&mut fixture, &mut reconnected).await;
    assert_parity(&fixture, &session, &reconnected, &requests, "reopened").await;
    assert_eq!(reconnected.keys().collect::<Vec<_>>(), before.keys().collect::<Vec<_>>());
    for (id, window) in &reconnected {
        assert_eq!(window.order, before[id].order, "{id} after reopen");
    }

    // A person's session sees only that person's attention, and cannot select another's.
    let avery = ClientSession::local(Some("person/avery")).unwrap();
    let mut own = Fixture::open_as(reopened.clone(), avery.clone(), true, None, &[]).await;
    own.send(json!({"kind":"subscribe","id":"mine","collection":"attention","limit":200})).await;
    own.send(json!({"kind":"subscribe","id":"theirs","collection":"attention","person":"person/robin","limit":200})).await;
    let mut frames = BTreeMap::new();
    for _ in 0..2 {
        let frame = own.frame().await;
        frames.insert(frame["id"].as_str().unwrap().to_owned(), frame);
    }
    assert_eq!(frames["theirs"]["kind"], "error", "{}", frames["theirs"]);
    let mut mine = BTreeMap::new();
    apply(&mut mine, &frames["mine"]);
    assert_eq!(mine["mine"].order, reconnected["avery"].order);
    assert_parity(&own, &avery, &mine,
        &[json!({"kind":"subscribe","id":"mine","collection":"attention","limit":200})], "person").await;
}

/// A session paired from another device: the clock rereads every window it holds.
fn paired_session() -> ClientSession {
    ClientSession {
        actor: "person/avery/session/phone".into(),
        authority_actor: "person/avery".into(),
        pairing_grant: None,
        transport: "paired",
        custom_forms: false,
        conversation_blocks: false,
        scopes: ["read.projections".into()].into_iter().collect(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_held_view_that_is_not_ready_resyncs_once_and_waits_for_its_next_change() {
    use std::sync::atomic::Ordering::SeqCst;
    for session in [ClientSession::local(None).unwrap(), paired_session()] {
        let paired = session.transport != "unix";
        let root = tempfile::tempdir().unwrap();
        let state = super::tests::test_state_named(root.path(), "alder");
        state.store.hold_collection_view("work");
        let gate = Arc::new(std::sync::atomic::AtomicU8::new(NOT_READY));
        let mut fixture =
            Fixture::open_with(state.clone(), session.clone(), false, None, Some(gate.clone()), &["work"]).await;
        let first = fixture.frame().await;
        assert_eq!((first["kind"].as_str(), first["retryable"].as_bool()), (Some("resync"), Some(true)), "{first}");
        // Neither a commit nor the reread interval reads it again meanwhile. A paired session's
        // clock does, to recheck its grant, and sends nothing.
        fixture.claim("daemon/fixture", "daemon.diagnostic",
            json!({"code":"fixture", "severity":"error", "reason":"unrelated"}));
        tokio::time::advance(ATTENTION_CLOCK_INTERVAL + COLLECTION_REREAD_INTERVAL).await;
        fixture.quiet().await;
        let clock_reads = usize::from(paired);
        assert_eq!(fixture.counts(), counts(&[("work", 1 + clock_reads)]), "{}", session.transport);
        // The view's next change reads it, and the window gets its rows.
        gate.store(READY, SeqCst);
        state.store.publish_collection_view("work");
        assert_eq!(fixture.frame().await["kind"], "snapshot");
        let reads = fixture.counts()["work"];
        // Withdrawn again: one resync, then quiet until the view changes once more.
        gate.store(NOT_READY, SeqCst);
        state.store.hold_collection_view("work");
        assert_eq!(fixture.frame().await["kind"], "resync");
        fixture.claim("daemon/fixture", "daemon.diagnostic",
            json!({"code":"fixture", "severity":"error", "reason":"unrelated again"}));
        tokio::time::advance(ATTENTION_CLOCK_INTERVAL + COLLECTION_REREAD_INTERVAL).await;
        fixture.quiet().await;
        assert_eq!(fixture.counts()["work"], reads + 1 + clock_reads, "{}", session.transport);
        if paired {
            // A grant revoked while the window waits ends it at the clock's next recheck, and it
            // gets no rows after.
            gate.store(REVOKED, SeqCst);
            tokio::time::advance(ATTENTION_CLOCK_INTERVAL).await;
            let revoked = fixture.frame().await;
            assert_eq!((revoked["kind"].as_str(), revoked["retryable"].as_bool()), (Some("error"), Some(false)), "{revoked}");
            gate.store(READY, SeqCst);
            state.store.publish_collection_view("work");
            fixture.quiet().await;
        } else {
            gate.store(READY, SeqCst);
            state.store.publish_collection_view("work");
            // Its rows changed meanwhile: the read count is in them.
            assert_eq!(fixture.frame().await["kind"], "changes");
        }
    }
}

#[tokio::test]
async fn a_held_view_still_retries_any_other_failed_read_on_the_interval() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    state.store.hold_collection_view("work");
    let mut fixture = Fixture::open(state, false, Some("work"), &["work"]).await;
    assert_eq!(fixture.frame().await["kind"], "resync");
    let recovered = fixture.frame().await;
    assert_eq!(recovered["kind"], "snapshot", "{recovered}");
    assert_eq!(fixture.counts(), counts(&[("work", 2)]));
}

/// A run with an agentless step and an agent's step, both ready: rows in the work list.
fn ready_work(store: &Store) {
    let source = r#"version 2
mission "orchard-chores" state="ready" {
  goal "Keep the orchard's work list honest."
  step "rake" { agentless; goal "Rake." }
  step "pick" { assigned-to "agent/alder.plain"; goal "Pick." }
}"#;
    let intent = crate::graph::parse_intent(source, "alder").unwrap();
    let planned = store.mission(&intent, crate::model::IntentInput { kdl: source.into(), source_name: None }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "orchard-chores").unwrap();
    let run = store.create_mission_run(&crate::model::MissionRunRequest {
        mission: "orchard-chores".into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/avery".into()), mode: Some("run".into()),
        inputs: BTreeMap::new(), idempotency_key: "orchard-chores-run".into(),
    }).unwrap();
    for step in &run.steps {
        store.set_step_state(&step.subject, "ready", None).unwrap();
    }
}

#[tokio::test]
async fn a_real_work_window_waits_for_the_published_list_and_ends_with_its_refresher() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "alder");
    ready_work(&state.store);
    let list = state.store.published_work_list();
    state.store.hold_collection_view("work");
    list.start();
    let direct = state.store.direct_work_reads();
    let mut fixture = Fixture::open(state.clone(), true, None, &["work"]).await;
    let first = fixture.frame().await;
    assert_eq!(first["kind"], "resync", "cold: {first}");
    fixture.claim("daemon/fixture", "daemon.diagnostic",
        json!({"code":"fixture", "severity":"error", "reason":"unrelated"}));
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("work", 1)]));
    // The first publication delivers its rows.
    super::super::published_lists::refresh_work_once(&state.store);
    let snapshot = fixture.frame().await;
    assert_eq!(snapshot["kind"], "snapshot", "{snapshot}");
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 2);
    // A forget: one resync at most, then the refold's rows.
    state.store.forget_current_views();
    let forgotten = fixture.frame().await;
    assert_eq!(forgotten["kind"], "resync", "{forgotten}");
    super::super::published_lists::refresh_work_once(&state.store);
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("work", 4)]), "the refold reread it once, with nothing new");
    assert_eq!(state.store.direct_work_reads(), direct, "no window read the list directly");
    // The refresher ends: one error that says so, and the subscription is gone.
    list.end();
    state.store.withdraw_collection_view("work");
    let ended = fixture.frame().await;
    assert_eq!((ended["kind"].as_str(), ended["retryable"].as_bool()), (Some("error"), Some(false)), "{ended}");
    assert!(ended["message"].as_str().unwrap().contains("work-list-ended"), "{ended}");
    fixture.claim("daemon/fixture", "daemon.diagnostic",
        json!({"code":"fixture", "severity":"error", "reason":"after the end"}));
    state.store.publish_collection_view("work");
    fixture.quiet().await;
    assert_eq!(fixture.counts(), counts(&[("work", 5)]), "read once for the end, then removed");
    assert_eq!(state.store.direct_work_reads(), direct);
}
