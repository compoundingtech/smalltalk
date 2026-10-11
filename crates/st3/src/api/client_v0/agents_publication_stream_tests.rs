use super::*;
use crate::store::agents_publication::{AgentsStatusWatermark, Prepared};
use tokio_tungstenite::tungstenite::Message;

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server { fn drop(&mut self) { self.0.abort(); } }

fn row(index: usize, text: &str) -> Value {
    json!({"id":format!("agent/stream-{index:04}"),"kind":"agent","revision":"claim/test",
        "updated_at":"2026-10-10T12:00:00.000Z","name":format!("Agent {index:04}"),
        "state":"stopped","reachability":"local","runtime_ids":[],"detail":text})
}

fn publish(state: &AppState, cut: u64, rows: Vec<Value>) -> Arc<crate::store::agents_publication::AgentsPublication> {
    let prepared = Prepared::new(AgentsStatusWatermark {store_index:cut, local_frontier:cut},
        1_791_633_600_000 + cut, None, rows).unwrap();
    state.store.finish_agents_publication(state.store.agents_publication_build(), prepared).unwrap();
    state.store.agents_publication().unwrap()
}

async fn connect(state: AppState, session: ClientSession) -> (Socket, Server) {
    use std::os::fd::AsRawFd as _;
    let app = axum::Router::new().route("/stream", axum::routing::get(move |upgrade: WebSocketUpgrade| {
        let (state, session) = (state.clone(), session.clone());
        async move {
            upgrade.on_upgrade(move |socket| collection_stream_socket_with_reader(socket, state, session, None,
                |state, _session, _request, _permit| async move {
                    Ok((new_client_snapshot(&state), vec![json!({"id":"work/other"})], false))
                }))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    super::super::tests::agents_stream_bench_buffer(listener.as_raw_fd(), libc::SO_SNDBUF);
    let address = listener.local_addr().unwrap();
    let server = Server(tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); }));
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/stream")).await.unwrap();
    if let tokio_tungstenite::MaybeTlsStream::Plain(stream) = socket.get_ref() {
        super::super::tests::agents_stream_bench_buffer(stream.as_raw_fd(), libc::SO_RCVBUF);
    }
    (socket, server)
}

async fn command(socket: &mut Socket, command: Value) {
    socket.send(Message::Text(command.to_string().into())).await.unwrap();
}

async fn frame(socket: &mut Socket) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let Message::Text(text) = message {
                assert!(text.len() <= CLIENT_MAX_RESPONSE_BYTES);
                return serde_json::from_str(&text).unwrap();
            }
        }
    }).await.expect("the subscribed publication must arrive")
}

async fn subscribe(socket: &mut Socket) {
    command(socket, json!({"kind":"subscribe","id":"agents","collection":"agents",
        "agents_publication_version":1})).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_chunked_complete_roster_is_contiguous_on_mixed_socket() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let rows = (0..401).map(|index| row(index, &"escaped \"multibyte λ\\\n".repeat(300))).collect::<Vec<_>>();
    let target = publish(&state, 1, rows);
    let builds = state.store.agent_resources_builds_for_test();
    let (mut socket, _server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    command(&mut socket, json!({"kind":"subscribe","id":"other","collection":"work","limit":1})).await;
    let mut collected = Vec::new();
    let mut first: Option<Value> = None;
    let mut other = false;
    loop {
        let current = frame(&mut socket).await;
        if current["id"] == "other" {
            assert!(first.is_none() || collected.len() == 401, "other data cannot interrupt a snapshot sequence");
            other = true;
        } else {
            let initial = first.get_or_insert_with(|| current.clone());
            assert_eq!(current["kind"], "snapshot");
            assert_eq!(current["has_more"], false);
            assert_eq!(current["publication"], initial["publication"]);
            assert_eq!(current["snapshot"], initial["snapshot"]);
            assert_eq!(current["chunk_count"], initial["chunk_count"]);
            assert!(current["chunk_count"].as_u64().unwrap() > 1);
            let items = current["items"].as_array().unwrap();
            assert_eq!(current["order"], json!(items.iter().map(|row| &row["id"]).collect::<Vec<_>>()));
            collected.extend(items.iter().cloned());
            if current["chunk_index"].as_u64().unwrap() + 1 == current["chunk_count"].as_u64().unwrap() {
                assert_eq!(collected.as_slice(), target.rows());
            }
        }
        if other && collected.len() == 401 { break; }
    }
    assert_eq!(state.store.agent_resources_builds_for_test(), builds, "socket subscribers must not refold Store cards");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_bursts_skip_to_latest_and_deliver_empty_metadata_delta() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let initial = publish(&state, 1, vec![row(0, "initial"), row(1, "removed")]);
    let (mut socket, _server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    assert_eq!(frame(&mut socket).await["publication"]["revision"], initial.metadata().revision);
    publish(&state, 2, vec![row(0, "intermediate")]);
    let latest = publish(&state, 3, vec![row(0, "latest")]);
    let started = std::time::Instant::now();
    let changed = frame(&mut socket).await;
    assert!(started.elapsed() < Duration::from_millis(250), "real publications bypass held-window debounce");
    assert_eq!(changed["kind"], "changes");
    assert_eq!(changed["base_revision"], initial.metadata().revision);
    assert_eq!(changed["publication"]["revision"], latest.metadata().revision);
    assert_eq!(changed["removes"], json!(["agent/stream-0001"]));
    assert_eq!(changed["order"], json!(latest.order()));
    let metadata_only = publish(&state, 4, latest.rows().to_vec());
    let changed = frame(&mut socket).await;
    assert_eq!(changed["base_revision"], latest.metadata().revision);
    assert_eq!(changed["publication"]["revision"], metadata_only.metadata().revision);
    assert_eq!(changed["upserts"], json!([]));
    assert_eq!(changed["removes"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_replacement_reconnect_and_epoch_reset_force_full_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let initial = publish(&state, 1, vec![row(0, "initial")]);
    let (mut socket, _server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    frame(&mut socket).await;
    subscribe(&mut socket).await;
    assert_eq!(frame(&mut socket).await["kind"], "snapshot", "replacement has no replay cursor");
    state.store.forget_current_views();
    let reset = publish(&state, 2, vec![row(0, "reset")]);
    assert_ne!(initial.metadata().node_epoch, reset.metadata().node_epoch);
    let snapshot = frame(&mut socket).await;
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["publication"]["node_epoch"], reset.metadata().node_epoch.to_string());
    socket.close(None).await.unwrap();
    let (mut socket, _reconnected_server) = connect(state, ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    assert_eq!(frame(&mut socket).await["kind"], "snapshot", "reconnect always starts from a complete current view");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_oversized_row_refuses_before_any_chunk_and_legacy_stays_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    publish(&state, 1, vec![row(0, "fits"), row(1, &"x".repeat(CLIENT_MAX_RESPONSE_BYTES))]);
    let (mut socket, _server) = connect(state, ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    let error = frame(&mut socket).await;
    assert_eq!(error["kind"], "error");
    assert_eq!(error["code"], "agents-publication-row-too-large");
    assert_eq!(error["retryable"], false);
    command(&mut socket, json!({"kind":"subscribe","id":"legacy","collection":"agents","limit":1})).await;
    let legacy = frame(&mut socket).await;
    assert_eq!(legacy["kind"], "snapshot");
    for field in ["publication", "base_revision", "chunk_index", "chunk_count"] {
        assert!(legacy.get(field).is_none(), "legacy frames must not receive {field}");
    }
}

#[test]
fn revisioned_agents_rejects_filtered_or_wrong_version_commands() {
    for extra in [json!({"limit":200}), json!({"status":"running"}), json!({"actor":"person/a"}),
        json!({"person":"person/a"}), json!({"subject":"agent/a"}), json!({"agents_publication_version":2}),
        json!({"collection":"work"})]
    {
        let mut request = json!({"kind":"subscribe","id":"agents","collection":"agents","agents_publication_version":1});
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let request = serde_json::from_value(request).unwrap();
        assert!(agents_publication_stream::validate(&request).is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_cached_bytes_revalidate_grant_expiry_revocation_and_scope() {
    for denied in ["expired", "revoked", "ungranted"] {
        let root = tempfile::tempdir().unwrap();
        let state = tests::test_state_named(root.path(), "node");
        let pairing = |expires: u64, scopes: Vec<&str>| ClaimInput {
            subject: "custom/client/stream-test".into(), kind: "custom.client.pairing-completed".into(),
            actor: Some("person/ada".into()), fields: serde_json::from_value(json!({
                "session_actor":"client/stream-test","person_id":"person/ada",
                "credential_hash":"invented-test-credential","expires_at_unix_ms":expires,
                "scopes":scopes,
            })).unwrap(), evidence:Vec::new(), expected_subject:None, idempotency_key:None,
        };
        let paired = state.store.append_claim(&pairing(u64::MAX, vec!["read.projections"])).unwrap();
        let session = paired_client_session(&state, &paired, "fabric-loopback", false).unwrap();
        publish(&state, 1, vec![row(0, "initial")]);
        let (mut socket, _server) = connect(state.clone(), session.clone()).await;
        subscribe(&mut socket).await;
        assert_eq!(frame(&mut socket).await["kind"], "snapshot");
        match denied {
            "expired" => { state.store.append_claim(&pairing(1, vec!["read.projections"])).unwrap(); }
            "ungranted" => { state.store.append_claim(&pairing(u64::MAX, Vec::new())).unwrap(); }
            _ => { state.store.append_claim(&ClaimInput {
                subject:"custom/client/stream-test".into(),kind:"custom.client.pairing-revoked".into(),
                actor:Some("person/ada".into()),fields:Default::default(),evidence:Vec::new(),
                expected_subject:None,idempotency_key:None,
            }).unwrap(); }
        }
        let publication = publish(&state, 2, vec![row(0, "must not escape")]);
        let _ = publication.encoded();
        let error = frame(&mut socket).await;
        assert_eq!(error["kind"], "error", "{denied} must stop cached delivery");
        assert_eq!(error["retryable"], false);
        assert!(error.get("items").is_none() && error.get("upserts").is_none());
        let (mut reconnected, _reconnected_server) = connect(state, session).await;
        subscribe(&mut reconnected).await;
        assert_eq!(frame(&mut reconnected).await["kind"], "error", "reconnect revalidates {denied}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_old_preparation_success_cannot_lose_the_final_quiet_input() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let first = publish(&state, 1, vec![row(0, "first")]);
    let held = agents_publication_stream::HeldPreparation::new(&first);
    let (mut socket, _server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    tokio::time::timeout(Duration::from_secs(5), held.entered.notified()).await.unwrap();
    publish(&state, 2, vec![row(0, "second")]);
    let third = publish(&state, 3, vec![row(0, "third and quiet")]);
    // Let the notice deadline fire while the first preparation is held.
    tokio::time::sleep(Duration::from_millis(75)).await;
    held.release.notify_one();
    let snapshot = frame(&mut socket).await;
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["publication"]["revision"], first.metadata().revision);
    let changed = frame(&mut socket).await;
    assert_eq!(changed["kind"], "changes");
    assert_eq!(changed["base_revision"], first.metadata().revision);
    assert_eq!(changed["publication"]["revision"], third.metadata().revision);
    assert_eq!(changed["upserts"], json!(third.rows()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_unsubscribe_cancels_preparation_and_releases_retired_roster() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let first = publish(&state, 1, vec![row(0, "held")]);
    let retired = Arc::downgrade(&first);
    let held = agents_publication_stream::HeldPreparation::new(&first);
    let (mut socket, _server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut socket).await;
    tokio::time::timeout(Duration::from_secs(5), held.entered.notified()).await.unwrap();
    publish(&state, 2, vec![row(0, "current")]);
    drop(first);
    command(&mut socket, json!({"kind":"unsubscribe","id":"agents"})).await;
    // A subsequent ordinary snapshot acknowledges that the unsubscribe was processed.
    command(&mut socket, json!({"kind":"subscribe","id":"other","collection":"work","limit":1})).await;
    assert_eq!(frame(&mut socket).await["id"], "other");
    tokio::time::timeout(Duration::from_millis(200), async {
        while retired.upgrade().is_some() { tokio::task::yield_now().await; }
    }).await.expect("cancellation must release the old complete publication without opening the preparation gate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revisioned_agents_non_reader_times_out_releases_retired_roster_and_does_not_block_other_sockets() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::test_state_named(root.path(), "node");
    let detail = "blocked reader ".repeat(2_000);
    let first = publish(&state, 1, (0..401).map(|index| row(index, &detail)).collect());
    let retired = Arc::downgrade(&first);
    let held = agents_publication_stream::HeldPreparation::new(&first);
    let (mut stalled, _stalled_server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut stalled).await;
    tokio::time::timeout(Duration::from_secs(5), held.entered.notified()).await.unwrap();
    let current = publish(&state, 2, vec![row(0, "current")]);
    drop(first);
    let started = tokio::time::Instant::now();
    held.release.notify_one();
    let (mut healthy, _healthy_server) = connect(state.clone(), ClientSession::local(None).unwrap()).await;
    subscribe(&mut healthy).await;
    let snapshot = tokio::time::timeout(Duration::from_millis(200), frame(&mut healthy)).await
        .expect("a blocked socket must not delay a healthy socket");
    assert_eq!(snapshot["publication"]["revision"], current.metadata().revision);
    assert!(retired.upgrade().is_some(), "the non-reader must still pin its blocked in-flight target");
    tokio::time::timeout(COLLECTION_SEND_TIMEOUT + Duration::from_secs(1), async {
        while retired.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("the whole-sequence timeout must close the socket and release its retired publication");
    assert!(started.elapsed() >= COLLECTION_SEND_TIMEOUT - Duration::from_millis(100),
        "the target must be released by the timeout, not by a completed send");
    let (mut reconnected, _reconnected_server) = connect(state, ClientSession::local(None).unwrap()).await;
    subscribe(&mut reconnected).await;
    let snapshot = frame(&mut reconnected).await;
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["publication"]["revision"], current.metadata().revision);
    assert_eq!(snapshot["items"], json!(current.rows()));
}

#[tokio::test(start_paused = true)]
async fn revisioned_agents_burst_coalescing_has_a_non_sliding_fifty_millisecond_deadline() {
    let first_notice = tokio::time::Instant::now();
    let mut due = None;
    agents_publication_stream::schedule_notice(&mut due);
    let fixed = due.unwrap();
    assert_eq!(fixed - first_notice, Duration::from_millis(50));
    for _ in 0..4 {
        tokio::time::advance(Duration::from_millis(10)).await;
        agents_publication_stream::schedule_notice(&mut due);
        assert_eq!(due, Some(fixed), "a new target replaces pending work without extending its deadline");
    }
    tokio::time::sleep_until(fixed).await;
    assert_eq!(tokio::time::Instant::now() - first_notice, Duration::from_millis(50));
}

#[test]
fn revisioned_agents_concurrent_subscribers_share_one_first_demand() {
    let owner = Arc::new(crate::store::agents_publication::Owner::default());
    let start = Arc::new(std::sync::Barrier::new(16));
    let callers = (0..16).map(|_| {
        let (owner, start) = (owner.clone(), start.clone());
        std::thread::spawn(move || { start.wait(); owner.subscribe() })
    }).collect::<Vec<_>>();
    let subscribed = callers.into_iter().map(|caller| caller.join().unwrap()).collect::<Vec<_>>();
    assert_eq!(subscribed.iter().filter(|(_, first)| *first).count(), 1);
    drop(subscribed);
    assert!(owner.subscribe().1, "a later first subscriber starts demand again after idle");
}
