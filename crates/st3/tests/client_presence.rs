use st3::{api::AppState, store::Store};
use st3_client::{Client, CollectionEvent};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Notify, watch};

fn state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("presence-test").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "presence-test".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
    }
}

/// The clients st lists: a client that names itself and one that does not, a stream that
/// appears with what it follows and leaves when it closes. Listing only: nobody is refused.
#[tokio::test]
async fn clients_list_shows_streams_appearing_and_leaving_and_the_header_is_optional() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let app = st3::api::router(state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix_as(&socket, "person/ada");
    // No name yet: the request is served, and the client is listed without one.
    client.capabilities().await.unwrap();
    let listed = client.clients_list().await.unwrap().value;
    assert_eq!(listed.kind, "client-connections");
    let unnamed = listed
        .items
        .iter()
        .find(|item| item.actor == "person/ada" && item.client.is_none())
        .unwrap_or_else(|| panic!("{listed:?}"));
    assert_eq!((unnamed.via.as_str(), unnamed.connected), ("local", false));

    // A named client's stream is connected while it is open, with what it follows.
    st3_client::set_client_name("presence-test 1.0+abc");
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe_glasses("glasses").await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(first, CollectionEvent::Snapshot { .. }),
        "{first:?}"
    );
    let named = |listed: &st3_client::ClientConnections| {
        listed
            .items
            .iter()
            .find(|item| item.client.as_deref() == Some("presence-test 1.0+abc"))
            .cloned()
    };
    let listed = client.clients_list().await.unwrap().value;
    let row = named(&listed).unwrap_or_else(|| panic!("{listed:?}"));
    assert!(row.connected && row.streams == 1, "{row:?}");
    assert_eq!(row.follows, ["glasses"]);
    assert_eq!(row.person, "person/ada");

    // Closed, it is no longer connected; it stays listed as seen for a few minutes.
    stream.close().await;
    let mut left = None;
    for _ in 0..100 {
        let listed = client.clients_list().await.unwrap().value;
        if let Some(row) = named(&listed).filter(|row| !row.connected) {
            left = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let left = left.expect("the closed stream leaves");
    assert_eq!(left.streams, 0);
    assert!(left.follows.is_empty());
    server.abort();
}

/// A quiet collection stream proves it is alive without a request: a WebSocket ping is answered,
/// and the answer counts as heard. This is what lets a client stop asking st "are you there" on a
/// timer (Nathan, 2026-10-06: no polling).
#[tokio::test]
async fn a_collection_stream_answers_a_websocket_ping_without_a_request() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let app = st3::api::router(state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix_as(&socket, "person/ada");
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe_glasses("glasses").await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(first, CollectionEvent::Snapshot { .. }), "{first:?}");
    let heard = stream.heard();
    // Quiet: nothing arrives while nothing changes.
    let quiet = tokio::time::timeout(Duration::from_millis(1500), stream.next_event()).await;
    assert!(quiet.is_err(), "nothing was expected: {quiet:?}");
    assert!(heard.quiet_for() >= Duration::from_millis(1200), "{:?}", heard.quiet_for());
    // A ping is answered: what it brings back is no frame, only the pong that resets the quiet.
    stream.ping().await.unwrap();
    // The pong is read as the stream is waited on, as the feed does, and is no event.
    let answer = tokio::time::timeout(Duration::from_millis(600), stream.next_event()).await;
    assert!(answer.is_err(), "a pong is not an event: {answer:?}");
    assert!(
        heard.quiet_for() < Duration::from_millis(1000),
        "the daemon did not answer the ping: quiet for {:?}",
        heard.quiet_for()
    );
    server.abort();
}

/// A utility that asks to see its own timings gets routes without queries, statuses, sizes and
/// durations, and the first frame of a subscription with how long it took. Nothing is added to
/// what the client sends.
#[tokio::test]
async fn an_observer_sees_request_and_frame_timings_without_contents() {
    install_observer();
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let app = st3::api::router(state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix_as(&socket, "person/ada");
    client.usage_period(Some(1), Some(2)).await.unwrap();
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe_glasses("timing-glasses").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let seen = SEEN.lock().unwrap().join("\n");
    assert!(seen.contains("outcome: Completed"), "{seen}");
    assert!(seen.contains("route: \"/v1/client/usage\""), "{seen}");
    assert!(!seen.contains('?') && !seen.contains("since_ms"), "{seen}");
    assert!(seen.contains("status: Some(200)"), "{seen}");
    assert!(
        seen.contains("id: \"timing-glasses\", kind: \"snapshot\"") && seen.contains("since_subscribe: Some("),
        "{seen}"
    );
    server.abort();
}

static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// One process-wide observer for these tests, which record into `SEEN`.
fn install_observer() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        st3_client::set_observer(|observation| {
            SEEN.lock().unwrap().push(format!("{observation:?}"));
        });
    });
}

/// A request its caller stops waiting for is not lost: it is reported once as cancelled, with
/// the time it had run, and never as a completion.
#[tokio::test]
async fn an_observer_keeps_a_request_the_caller_cancelled_apart_from_completions() {
    install_observer();
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("silent.sock");
    // A server that accepts and never answers.
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((connection, _)) = listener.accept().await {
            open.push(connection);
        }
    });
    let client = Client::unix_as(&socket, "person/ada");
    let waited = tokio::time::timeout(Duration::from_millis(150), client.usage_period(Some(11), Some(12))).await;
    assert!(waited.is_err(), "the server never answers");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let seen = SEEN.lock().unwrap().clone();
    let mine = seen
        .iter()
        .filter(|line| line.contains("Request") && line.contains("/v1/client/usage") && line.contains("Cancelled"))
        .collect::<Vec<_>>();
    assert_eq!(mine.len(), 1, "{seen:#?}");
    assert!(mine[0].contains("status: None"), "{}", mine[0]);
    let took = mine[0].split("took: ").nth(1).unwrap();
    assert!(took.contains("ms"), "{}", mine[0]);
    // Nothing for that request was reported as completed.
    assert!(
        !seen.iter().any(|line| line.contains("/v1/client/usage") && line.contains("outcome: Completed") && line.contains("status: None")),
        "{seen:#?}"
    );
    held.abort();
}

/// First-frame timers follow the subscriptions: replaced, dropped by an unsubscribe, and bounded.
#[tokio::test]
async fn an_observer_keeps_first_frame_timers_only_for_live_subscriptions() {
    install_observer();
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let app = st3::api::router(state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        st3::api::serve_unix(&server_socket, app).await.unwrap();
    });
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::unix_as(&socket, "person/ada");
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe("a", "attention", 10, None, None, None).await.unwrap();
    stream.subscribe("a", "attention", 10, None, None, None).await.unwrap();
    assert_eq!(stream.observed_pending(), 1, "a replacement restarts the timer");
    stream.unsubscribe("a").await.unwrap();
    assert_eq!(stream.observed_pending(), 0, "an unsubscribe drops it");
    for index in 0..20 {
        stream.subscribe(&format!("never-{index}"), "attention", 10, None, None, None)
            .await
            .unwrap();
    }
    assert!(stream.observed_pending() <= 8, "bounded by the socket's subscriptions: {}", stream.observed_pending());
    server.abort();
}
