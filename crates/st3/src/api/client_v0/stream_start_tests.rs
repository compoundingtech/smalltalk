use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

#[tokio::test]
async fn cold_open_four_subscriptions_do_not_wait_for_slow_roster() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "stream-fixture");
    state.store.append_claim(&ClaimInput {
        subject: "agent/chat".into(), kind: "runtime.observed".into(),
        actor: Some("agent/chat".into()),
        fields: serde_json::from_value(json!({"status":"running", "runtime_id":"chat", "incarnation_id":"one"})).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    // Hold the real store-wide cold-roster admission, not a mocked reader. Other
    // collections still use the production shared-window and SQLite snapshot path.
    let slow_roster = state.store.admit_agent_resources("other").await;
    let app = axum::Router::new().route("/stream", axum::routing::get(move |upgrade: WebSocketUpgrade| {
        let state = state.clone();
        async move {
            let windows = collection_windows::Windows::attach(&state.store);
            upgrade.on_upgrade(move |socket| collection_stream_socket_with_reader(
                socket, state, ClientSession::local(None).unwrap(), None,
                move |state, session, request, permit| {
                    let windows = windows.clone();
                    async move { collection_items_with_windows(&state, &session, &request, permit, windows).await }
                },
            ))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream")).await.unwrap();
    let start = Instant::now();
    for command in [
        json!({"kind":"subscribe", "id":"agents", "collection":"agents", "limit":100}),
        json!({"kind":"subscribe", "id":"missions", "collection":"missions", "limit":100}),
        json!({"kind":"subscribe", "id":"attention", "collection":"attention", "limit":100}),
        json!({"kind":"subscribe", "id":"conversation", "collection":"conversation", "conversation":"agent/chat"}),
    ] {
        socket.send(tokio_tungstenite::tungstenite::Message::Text(command.to_string().into())).await.unwrap();
    }
    let mut received = BTreeSet::new();
    for _ in 0..3 {
        let raw = tokio::time::timeout(Duration::from_secs(5), socket.next()).await.unwrap().unwrap().unwrap();
        let frame: Value = serde_json::from_str(raw.to_text().unwrap()).unwrap();
        assert_ne!(frame["id"], "agents", "cold roster admission must remain held");
        assert_eq!(frame["kind"], if frame["id"] == "conversation" { "conversation" } else { "snapshot" });
        println!("cold-open independent first frame: {} {:.3} ms", frame["id"], start.elapsed().as_secs_f64() * 1000.0);
        received.insert(frame["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(received, BTreeSet::from(["missions".into(), "attention".into(), "conversation".into()]));
    // Make the injected delay visible in the timing output; the assertions above
    // already prove delivery without releasing the slow producer.
    tokio::time::sleep(Duration::from_millis(750)).await;
    let held_ms = start.elapsed().as_secs_f64() * 1000.0;
    drop(slow_roster);
    let raw = tokio::time::timeout(Duration::from_secs(5), socket.next()).await.unwrap().unwrap().unwrap();
    let frame: Value = serde_json::from_str(raw.to_text().unwrap()).unwrap();
    assert_eq!(frame["id"], "agents");
    assert_eq!(frame["kind"], "snapshot");
    println!("cold-open roster first frame: {:.3} ms (admission artificially held {:.3} ms)", start.elapsed().as_secs_f64() * 1000.0, held_ms);
    socket.close(None).await.unwrap();
    server.abort();
}

fn process_cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the structure on success.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) }, 0);
    let usage = unsafe { usage.assume_init() };
    let milliseconds = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    milliseconds(usage.ru_utime) + milliseconds(usage.ru_stime)
}

#[tokio::test]
#[ignore = "requires a scale-1 collection_fixture store; run alone with --ignored --nocapture"]
async fn cold_open_uncontended_snapshot_costs() {
    let root = tempfile::tempdir().unwrap();
    let path = std::path::PathBuf::from(std::env::var_os("ST_COLLECTION_FIXTURE")
        .expect("generate an invented store with the collection_fixture example"));
    assert!(path.is_file(), "the generated fixture must already exist");
    for collection in ["agents", "missions", "attention"] {
        // Reopen for each collection: neither its shared-window nor roster cache is
        // primed by another measurement. This is an invented fixture, never a live DB.
        let mut state = super::tests::test_state_named(root.path(), "bench-host");
        state.store = Arc::new(Store::open(&path, "bench-host").unwrap());
        assert!(state.store.index().unwrap() > 100_000, "use a scale-1 invented fixture");
        let windows = collection_windows::Windows::attach(&state.store);
        let session = ClientSession::local(Some("person/bench-operator")).unwrap();
        let request: CollectionSubscribe = serde_json::from_value(json!({
            "kind":"subscribe", "id":collection, "collection":collection, "limit":100,
        })).unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        for temperature in ["cold", "warm"] {
            let cpu = process_cpu_ms();
            let start = Instant::now();
            let (snapshot, items, has_more) = match collection_items_with_windows(
                &state, &session, &request, slots.clone().acquire_owned().await.unwrap(), windows.clone(),
            ).await {
                Ok(read) => read,
                Err(error) => {
                    // This is a measurement, not a wall-clock budget assertion.
                    // Production subquery deadlines can expire under ambient load.
                    println!("cold-open uncontended collection={collection} cache={temperature} status=error elapsed_ms={:.3} cpu_ms={:.3} code={} message={:?}",
                        start.elapsed().as_secs_f64() * 1000.0, process_cpu_ms() - cpu,
                        error.code, error.message);
                    continue;
                }
            };
            assert!(items.len() >= if collection == "agents" { 100 } else { 30 },
                "use a sample-sized fixture with current missions and attention, not only history");
            let ready_ms = start.elapsed().as_secs_f64() * 1000.0;
            let order: Vec<_> = items.iter().map(|item| item["id"].clone()).collect();
            let frame = json!({"kind":"snapshot", "id":collection, "collection":collection,
                "snapshot":snapshot, "order":order, "items":items, "has_more":has_more});
            let bytes = serde_json::to_vec(&frame).unwrap();
            println!("cold-open uncontended collection={collection} cache={temperature} status=ok claims={} rows={} bytes={} ready_ms={ready_ms:.3} frame_ms={:.3} cpu_ms={:.3}",
                snapshot.store_index, frame["items"].as_array().unwrap().len(), bytes.len(),
                start.elapsed().as_secs_f64() * 1000.0, process_cpu_ms() - cpu);
            assert!(bytes.len() <= CLIENT_MAX_RESPONSE_BYTES);
        }
    }
}

#[derive(Clone, Copy)]
enum OwnerAfterFailure { Recovered, Unavailable, Stalled, SlowPage }

async fn remote_fixture(root: &Path, after: OwnerAfterFailure, failure_code: &str) -> (AppState, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    use axum::body::Bytes;
    use axum::response::IntoResponse as _;
    use std::os::unix::fs::PermissionsExt as _;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let failure_code = failure_code.to_owned();
    let auth = crate::peer::FleetAuth::test("stream-test", &[7; 32]);
    let app = axum::Router::new().route("/v1/peer/client-read", axum::routing::post(move |body: Bytes| {
        let (calls, auth, failure_code) = (count.clone(), auth.clone(), failure_code.clone());
        async move {
            let request: crate::peer::ClientReadRequest = serde_json::from_slice(&body).unwrap();
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let (status, value) = if matches!(after, OwnerAfterFailure::Unavailable) || attempt == 0 {
                (if failure_code == "remote-unavailable" { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::FORBIDDEN },
                    json!({"code":failure_code, "message":"owner transport is starting", "details":{"reason":"dial-failed"}}))
            } else if matches!(after, OwnerAfterFailure::Stalled) {
                return std::future::pending().await;
            } else {
                match request.request {
                    crate::peer::ClientReadOperation::ConversationChanges { after: None, .. } =>
                        (StatusCode::OK, json!({"items":[], "next_cursor":"cursor/ready"})),
                    crate::peer::ClientReadOperation::Timeline { .. } => {
                        if matches!(after, OwnerAfterFailure::SlowPage) {
                            tokio::time::sleep(Duration::from_secs(4)).await;
                        }
                        (StatusCode::OK, json!({"items":[{"id":"timeline-entry/hello", "body":{"text":"ready"}}], "page":{"has_more":false}}))
                    }
                    _ => return std::future::pending().await,
                }
            };
            let envelope = crate::model::ApiResponse {
                api_version: "st3.v1".into(), request_id: "request/fixture".into(),
                snapshot_host: "owner".into(), store_index: 0, value,
            };
            let bytes = serde_json::to_vec(&envelope).unwrap();
            let headers = auth.response_headers_for("/v1/peer/client-read", "owner", &bytes, &crate::peer::FleetAuth::body_digest(&body)).unwrap();
            let mut response = (status, bytes).into_response();
            response.headers_mut().extend(headers);
            response
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
    let secret = root.join("secret");
    std::fs::write(&secret, [7_u8; 32]).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut state = super::tests::test_state_named(root, "gateway");
    state.client_relay = crate::peer::ClientRelay::from_config(&crate::config::Config {
        node: "gateway".into(), fleet_id: Some("stream-test".into()), shared_secret_file: Some(secret),
        peers: vec![crate::config::PeerConfig { name: "owner".into(), url: format!("http://{address}") }],
        ..Default::default()
    }).unwrap();
    (state, calls, server)
}

#[tokio::test]
async fn cold_open_remote_transient_failure_does_not_force_resubscribe() {
    let root = tempfile::tempdir().unwrap();
    let (state, calls, server) = remote_fixture(root.path(), OwnerAfterFailure::Recovered, "remote-unavailable").await;
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    let follower = tokio::spawn(follow_conversation(state, ClientSession::local(Some("person/test")).unwrap(),
        "chat".into(), 1, ("session/fixture".into(), Some("host/owner".into())), outbox, Queue::new(Kind::Conversation), (None, None)));
    let (_, _, frame) = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await.unwrap().unwrap();
    println!("cold-open transient owner first frame: {} {:.3} ms, {} owner calls", frame["kind"], start.elapsed().as_secs_f64() * 1000.0, calls.load(Ordering::SeqCst));
    follower.abort();
    server.abort();
    assert_eq!(frame["kind"], "conversation", "a recovered initial transport must not prompt subscription replacement: {frame}");
    assert_eq!(frame["items"][0]["id"], "timeline-entry/hello");
    assert!(calls.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn cold_open_remote_outage_still_reports_resync() {
    let root = tempfile::tempdir().unwrap();
    let (state, calls, server) = remote_fixture(root.path(), OwnerAfterFailure::Unavailable, "remote-unavailable").await;
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    let follower = tokio::spawn(follow_conversation(state, ClientSession::local(Some("person/test")).unwrap(),
        "chat".into(), 1, ("session/fixture".into(), Some("host/owner".into())), outbox, Queue::new(Kind::Conversation), (None, None)));
    let (_, _, frame) = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await.unwrap().unwrap();
    let elapsed = start.elapsed();
    println!("cold-open persistent outage first frame: {} {:.3} ms, {} owner calls", frame["kind"], elapsed.as_secs_f64() * 1000.0, calls.load(Ordering::SeqCst));
    // The initial grace is spent once. A continued outage uses the ordinary
    // retained-subscription retry interval, rather than restarting the grace.
    let (_, _, next) = tokio::time::timeout(Duration::from_secs(3), frames.recv()).await.unwrap().unwrap();
    assert_eq!(next["kind"], "resync");
    follower.abort();
    server.abort();
    assert_eq!(frame["kind"], "resync");
    assert_eq!(frame["code"], "remote-unavailable");
    assert_eq!(frame["retryable"], true);
    assert!(elapsed >= Duration::from_secs(3), "initial transport grace ended prematurely");
}

#[tokio::test]
async fn cold_open_remote_authorization_failure_is_not_retried() {
    let root = tempfile::tempdir().unwrap();
    let (state, calls, server) = remote_fixture(root.path(), OwnerAfterFailure::Unavailable, "forbidden").await;
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let follower = tokio::spawn(follow_conversation(state, ClientSession::local(Some("person/test")).unwrap(),
        "chat".into(), 1, ("session/fixture".into(), Some("host/owner".into())), outbox, Queue::new(Kind::Conversation), (None, None)));
    let (_, _, frame) = tokio::time::timeout(Duration::from_secs(5), frames.recv()).await.unwrap().unwrap();
    follower.abort();
    server.abort();
    assert_eq!(frame["kind"], "error");
    assert_eq!(frame["code"], "forbidden");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cold_open_remote_stalled_retry_keeps_normal_peer_read_deadline() {
    let root = tempfile::tempdir().unwrap();
    let (state, calls, server) = remote_fixture(root.path(), OwnerAfterFailure::Stalled, "remote-unavailable").await;
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    let follower = tokio::spawn(follow_conversation(state, ClientSession::local(Some("person/test")).unwrap(),
        "chat".into(), 1, ("session/fixture".into(), Some("host/owner".into())), outbox, Queue::new(Kind::Conversation), (None, None)));
    let (_, _, frame) = tokio::time::timeout(Duration::from_secs(20), frames.recv()).await.unwrap().unwrap();
    println!("cold-open stalled retry first frame: {} {:.3} ms, {} owner calls", frame["kind"], start.elapsed().as_secs_f64() * 1000.0, calls.load(Ordering::SeqCst));
    follower.abort();
    server.abort();
    assert_eq!(frame["kind"], "resync");
    assert_eq!(frame["code"], "remote-unavailable");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(start.elapsed() >= Duration::from_secs(15));
    assert!(frame["message"].as_str().unwrap().contains("timed-out"), "{frame}");
}

#[tokio::test]
async fn cold_open_remote_slow_successful_retry_is_not_canceled() {
    let root = tempfile::tempdir().unwrap();
    let (state, calls, server) = remote_fixture(root.path(), OwnerAfterFailure::SlowPage, "remote-unavailable").await;
    let (outbox, mut frames) = tokio::sync::mpsc::unbounded_channel();
    let start = Instant::now();
    let follower = tokio::spawn(follow_conversation(state, ClientSession::local(Some("person/test")).unwrap(),
        "chat".into(), 1, ("session/fixture".into(), Some("host/owner".into())), outbox, Queue::new(Kind::Conversation), (None, None)));
    let (_, _, frame) = tokio::time::timeout(Duration::from_secs(6), frames.recv()).await.unwrap().unwrap();
    println!("cold-open slow successful retry first frame: {} {:.3} ms, {} owner calls", frame["kind"], start.elapsed().as_secs_f64() * 1000.0, calls.load(Ordering::SeqCst));
    follower.abort();
    server.abort();
    assert_eq!(frame["kind"], "conversation", "the retry-start window must not cancel an admitted successful page: {frame}");
    assert!(start.elapsed() >= Duration::from_secs(4));
}
