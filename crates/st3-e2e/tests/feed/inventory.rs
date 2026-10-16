use super::*;

#[tokio::test]
async fn inventories_follow_sources_switch_periods_leave_and_reconnect_over_a_real_socket() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("inventory.sock");
    let mut state = test_state(root.path());
    state.native_session_home = Some(root.path().join("native"));
    std::fs::create_dir_all(state.native_session_home.as_ref().unwrap()).unwrap();
    let app = st3::api::router(state);
    let served = socket.clone();
    let server = tokio::spawn(async move { st3::api::serve_unix(&served, app).await });
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
    let feed = tokio::spawn(run_members(
        vec![client.clone()],
        false,
        false,
        tx,
        command_receiver,
    ));
    let wanted = Inventories {
        clients: true,
        usage_hours: Some(24),
        machines: true,
        devices: true,
        sessions: true,
        terminals: true,
    };
    commands.send(Command::Inventories(wanted)).unwrap();
    let mut pending = wanted
        .windows()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut supported = false;
    while !pending.is_empty() {
        match rx.try_recv() {
            Ok(Update::InventorySupport { granted, .. }) => supported = granted,
            Ok(Update::Window {
                window: Window::Inventory(inventory),
                items,
                ..
            }) => {
                if inventory == Inventory::Usage(24) {
                    assert!(model::inventory_usage(items).is_ok());
                }
                pending.remove(&inventory);
            }
            Ok(Update::WindowFailed(window, error)) => panic!("{window:?}: {error}"),
            Ok(_) => {}
            Err(mpsc::TryRecvError::Empty) => {
                assert!(std::time::Instant::now() < deadline, "pending: {pending:?}");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(supported);
    // A second presence source changes the client window without a graph write.
    let mut other = client.collection_stream().await.unwrap();
    other
        .subscribe("other", "agents", WINDOW, None, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let rows = next_window(&rx, Window::Inventory(Inventory::Clients)).await;
            let clients = model::inventory_clients(rows, "host/stui-feed".into());
            if clients.items.iter().any(|row| row.streams >= 2) {
                break;
            }
        }
    })
    .await
    .unwrap();
    // The old usage ID is removed before the selected period takes its one reserved slot.
    let week = Inventories {
        usage_hours: Some(168),
        ..wanted
    };
    commands.send(Command::Inventories(week)).unwrap();
    let rows = next_window(&rx, Window::Inventory(Inventory::Usage(168))).await;
    let period = model::inventory_usage(rows).unwrap();
    assert!(period.until_ms - period.since_ms >= 167 * 3_600_000);
    commands
        .send(Command::Inventories(Inventories::default()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let follows = client
                .clients_list()
                .await
                .unwrap()
                .value
                .items
                .into_iter()
                .flat_map(|row| row.follows)
                .collect::<Vec<_>>();
            if !follows.iter().any(|name| {
                week.windows()
                    .iter()
                    .any(|window| name == window.collection())
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(commands);
    feed.await.unwrap();
    drop(other);
    // A fresh feed receives all authoritative snapshots again.
    let (tx, rx) = mpsc::channel();
    let (commands, receiver) = channel::unbounded_channel();
    let fresh = tokio::spawn(run_members(vec![client], false, false, tx, receiver));
    commands.send(Command::Inventories(wanted)).unwrap();
    assert!(
        model::inventory_usage(next_window(&rx, Window::Inventory(Inventory::Usage(24))).await)
            .is_ok()
    );
    drop(commands);
    fresh.await.unwrap();
    server.abort();
}
