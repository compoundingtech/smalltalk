//! The feed against a real st daemon over a real socket.

use st3_client::{Client, Resource};
use st3_feed::*;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc as channel;
use tokio::sync::{Notify, watch};

mod inventory;
pub fn test_state(root: &Path) -> st3::api::AppState {
    st3::api::AppState {
        owning_person: None,
        // Socket reads overlap graph writes, as on a real WAL-backed member.
        store: Arc::new(st3::store::Store::open(&root.join("graph.db"), "stui-feed").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "stui-feed".into(),
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

pub async fn next_window(updates: &mpsc::Receiver<Update>, wanted: Window) -> Vec<Resource> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match updates.try_recv() {
            Ok(Update::Window { window, items, .. }) if window == wanted => return items,
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no {wanted:?} window arrived"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
        }
    }
}

#[tokio::test]
async fn sidebar_arrangements_follow_the_explicit_person_and_remote_register_updates() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("sidebar.sock");
    let state = test_state(root.path());
    let app = st3::api::router(state.clone());
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    let client = Client::unix_as(&socket, "person/avery");
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.capabilities().await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let (commands, command_receiver) = channel::unbounded_channel();
    let feed = tokio::spawn(run_members_with_sidebar(
        vec![client],
        false,
        true,
        true,
        Some("person/avery".into()),
        tx,
        command_receiver,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut pending = std::collections::BTreeSet::from([
            Window::Attention,
            Window::Missions,
            Window::Agents,
            Window::Glasses,
            Window::Arrangements,
        ]);
        while !pending.is_empty() {
            match rx.try_recv() {
                Ok(Update::Window { window, items, .. }) if pending.remove(&window) => {
                    assert!(items.is_empty())
                }
                Ok(Update::WindowFailed(window, error)) => {
                    panic!("{window:?} refused: {error}")
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("feed stopped"),
            }
        }
    })
    .await
    .unwrap();
    let subject = "arrangement/person/avery/019a0000-0000-7000-8000-000000000001";
    let append = |operations| {
        state
            .store
            .append_client_claim(&st3::model::ClaimInput {
                subject: subject.into(),
                kind: "arrangement.edited".into(),
                actor: Some("person/avery".into()),
                fields: serde_json::from_value(
                    json!({"owner":"person/avery", "operations":operations}),
                )
                .unwrap(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        state
            .event_notify
            .send_replace(state.store.index().unwrap());
    };
    append(
        json!([{"op":"create","name":"Sidebar"}, {"op":"folder.create","id":"019a0000-0000-7000-8000-000000000010","name":"Builds","parent":null,"key":"a0"}]),
    );
    let items = next_window(&rx, Window::Arrangements).await;
    assert!(
        matches!(&items[0], Resource::Arrangement(a) if a.owner == "person/avery" && a.body.folders.values().next().unwrap().name.value == "Builds")
    );
    append(
        json!([{"op":"folder.rename","id":"019a0000-0000-7000-8000-000000000010","name":"Renamed"}]),
    );
    let items = next_window(&rx, Window::Arrangements).await;
    assert!(
        matches!(&items[0], Resource::Arrangement(a) if a.body.folders.values().next().unwrap().name.value == "Renamed")
    );
    append(json!([{"op":"retire"}]));
    assert!(next_window(&rx, Window::Arrangements).await.is_empty());
    drop(commands);
    tokio::time::timeout(Duration::from_secs(5), feed)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn an_unreachable_member_falls_back_and_announces_glasses_before_their_window() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("live.sock");
    let app = st3::api::router(test_state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let live = Client::unix_as(&socket, "person/avery");
    let (tx, rx) = mpsc::channel();
    let (commands, command_receiver) = channel::unbounded_channel();
    let feed = tokio::spawn(run_members(
        vec![
            Client::unix_as(root.path().join("absent.sock"), "person/avery"),
            live.clone(),
        ],
        false,
        true,
        tx,
        command_receiver,
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut connected = false;
        let mut version = None;
        loop {
            match rx.try_recv() {
                Ok(Update::Connected(client)) => {
                    assert_eq!(format!("{client:?}"), format!("{live:?}"));
                    connected = true;
                }
                Ok(Update::GlassesVersion(value)) => {
                    assert!(connected);
                    version = Some(value);
                }
                Ok(Update::Window {
                    window: Window::Glasses,
                    ..
                }) => {
                    assert!(version.is_some_and(|value| value >= 1));
                    break;
                }
                Ok(Update::Offline(reason)) => panic!("live member was available: {reason}"),
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
            }
        }
    })
    .await
    .unwrap();
    drop(commands);
    tokio::time::timeout(Duration::from_secs(5), feed)
        .await
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn the_missions_window_is_followed_only_when_asked_and_left_when_not_wanted() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let app = st3::api::router(test_state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (tx, rx) = mpsc::channel();
    let (commands, command_receiver) = channel::unbounded_channel();
    let feed = tokio::spawn(run_members_with(
        vec![Client::unix_as(&socket, "person/avery")],
        false,
        false,
        false,
        tx,
        command_receiver,
    ));
    // Started without missions: attention and agents arrive, missions never do.
    let mut seen = std::collections::BTreeSet::new();
    let wait = |want: Box<dyn Fn(&Update) -> bool>| {
        let rx = &rx;
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match rx.try_recv() {
                        Ok(update) if want(&update) => return update,
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => {
                            tokio::time::sleep(Duration::from_millis(10)).await
                        }
                        Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
                    }
                }
            })
            .await
            .expect("the update arrives")
        }
    };
    while seen.len() < 2 {
        if let Update::Window { window, .. } =
            wait(Box::new(|update| matches!(update, Update::Window { .. }))).await
        {
            assert_ne!(window, Window::Missions, "missions are not followed yet");
            seen.insert(window);
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(update) = rx.try_recv() {
        assert!(
            !matches!(update, Update::Window { window: Window::Missions, .. }),
            "no missions window while not followed"
        );
    }
    // Asked for, the window arrives; left, it is stopped and no longer current.
    commands.send(Command::Missions { follow: true }).unwrap();
    wait(Box::new(|update| matches!(update, Update::Window { window: Window::Missions, .. }))).await;
    commands.send(Command::Missions { follow: false }).unwrap();
    wait(Box::new(|update| matches!(update, Update::WindowStopped(Window::Missions)))).await;
    drop(commands);
    tokio::time::timeout(Duration::from_secs(5), feed).await.unwrap().unwrap();
    server.abort();
}

#[tokio::test]
async fn a_cursor_from_another_member_discards_the_timeline_and_reloads_snapshots() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let app = st3::api::router(test_state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let client = Client::unix_as(&socket, "person/avery");
    let mut model = model::Model::bootstrap(&client).await.unwrap();
    model.event_cursor = "event-cursor/previous-member/0".into();
    model.timeline.push(
        serde_json::from_value(serde_json::json!({
            "id":"entry/stale", "sequence":1, "revision":1,
            "timestamp":"2026-10-01T00:00:00Z", "role":"assistant", "final":true,
            "type":"content", "body":{"media_type":"text/plain", "text":"Stale transcript"}
        }))
        .unwrap(),
    );
    let (changed, sessions, gap) =
        tokio::time::timeout(Duration::from_secs(10), model.sync(&client))
            .await
            .unwrap()
            .unwrap();
    assert!(changed && gap);
    assert!(sessions.is_empty());
    assert!(model.timeline.is_empty());
    assert_eq!(
        model.event_cursor,
        client.capabilities().await.unwrap().value.event_cursor
    );
    assert!(model.machines.snapshot.is_some());
    assert_eq!(model.status, "Resynchronized after cursor gap");
    server.abort();
}

#[tokio::test]
async fn a_lost_stream_is_offline_only_when_st_does_not_answer_a_fresh_request() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let app = st3::api::router(test_state(root.path()));
    let server_socket = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let answering = link_lost(
        &Client::unix_as(&socket, "person/avery"),
        false,
        "st stopped answering",
    )
    .await;
    assert!(
        matches!(&answering, Update::Degraded(reason) if reason == "st stopped answering"),
        "st answered, so only its stream is replaced: {answering:?}"
    );
    // A paired device's gateway is never probed: its answer would prove nothing.
    let remote = link_lost(
        &Client::unix_as(&socket, "person/avery"),
        true,
        "the member stopped answering",
    )
    .await;
    assert!(
        matches!(&remote, Update::Offline(_)),
        "a lost remote stream is offline: {remote:?}"
    );
    server.abort();
    let _ = server.await;
    let gone = link_lost(
        &Client::unix_as(root.path().join("absent.sock"), "person/avery"),
        false,
        "st closed the connection",
    )
    .await;
    assert!(
        matches!(&gone, Update::Offline(reason) if reason == "st closed the connection"),
        "no answer at all is offline: {gone:?}"
    );
}

#[tokio::test]
async fn the_feed_waits_for_st_then_keeps_every_window_current_on_one_socket() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let state = test_state(root.path());
    let (tx, rx) = mpsc::channel();
    let (_commands, command_receiver) = channel::unbounded_channel();
    let feed = tokio::spawn(run(
        Client::unix_as(&socket, "person/avery"),
        tx,
        command_receiver,
    ));

    // No st yet: the feed says so and keeps trying.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.try_recv() {
            Ok(Update::Offline(_)) => break,
            Ok(other) => panic!("expected offline first, got {other:?}"),
            Err(_) => {
                assert!(std::time::Instant::now() < deadline, "no offline update");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    let server_socket = socket.clone();
    let app = st3::api::router(state.clone());
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });

    // Retain every initial window: independent server reads may finish in any order.
    // Waiting for one ID at a time would discard an already-arrived later window.
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut pending = std::collections::BTreeSet::from(Window::ALL);
        while !pending.is_empty() {
            match rx.try_recv() {
                Ok(Update::Window { window, items, .. }) if pending.remove(&window) => {
                    assert!(items.is_empty(), "initial {window:?} window was not empty");
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
            }
        }
    }).await.expect("all initial windows arrive independently");

    let source =
        "version 2\nmission \"feed-test\" state=\"ready\" { goal \"Push changes to stui\" }\n";
    let intent = st3::graph::parse_intent(source, "stui-feed").unwrap();
    let planned = state
        .store
        .mission(
            &intent,
            st3::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
        )
        .unwrap();
    state
        .store
        .apply(&intent, &planned.subject_tokens, "feed-test-definition")
        .unwrap();
    state
        .event_notify
        .send(state.store.index().unwrap())
        .unwrap();
    let missions = next_window(&rx, Window::Missions).await;
    assert_eq!(
        missions
            .iter()
            .map(|item| item.header().id.as_str())
            .collect::<Vec<_>>(),
        ["mission/feed-test"]
    );

    // Nothing changes, so nothing arrives: no polling.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rx.try_recv().is_err(), "an idle feed sends nothing");
    feed.abort();
    server.abort();
}
