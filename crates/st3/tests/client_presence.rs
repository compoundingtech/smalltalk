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
