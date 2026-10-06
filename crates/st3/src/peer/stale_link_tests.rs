/// Two listening daemons and a dial-out member with a replicated, obsolete up observation.
/// No fixture uses the running fleet, a public listener or an external transport tool.
#[tokio::test]
async fn stale_dial_out_links_never_offer_reverse_routes() {
    let fleet = "7c9e6679-7425-40de-944b-e07fc1f90ae7";
    let roots = [(); 3].map(|()| tempfile::tempdir().unwrap());
    let names = ["cedar", "birch", "fern"];
    let keys = [(); 3].map(|()| Arc::new(MemberKey::generate().unwrap().0));
    let states = std::array::from_fn::<_, 3, _>(|i| {
        let root = roots[i].path();
        let store = Arc::new(Store::open(&root.join("graph.db"), names[i]).unwrap());
        store.bind_fleet(fleet).unwrap();
        store.pin_fleet_anchor(keys[0].public()).unwrap();
        store.set_member_key(Some(keys[i].clone())).unwrap();
        crate::api::AppState {
            store,
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: names[i].into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: Some(fleet.into()),
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        }
    });
    states[0]
        .store
        .admit_fleet_anchor(fleet, keys[0].public(), "listening")
        .unwrap();
    for i in 1..3 {
        fleet_claim(
            &states[0].store,
            "fleet.member-admitted",
            &format!("host/{}", names[i]),
            serde_json::json!({
                "fleet_id": fleet, "member_key": keys[i].public(), "via": "invite",
                "mode": if i == 2 { "dial-out" } else { "listening" },
                "sponsor": "host/birch"
            }),
        );
    }
    for state in &states[1..] {
        let exchange = states[0]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        state
            .store
            .receive_replication_exchange("cedar", fleet, &exchange)
            .unwrap();
        state.store.validate_replication_backlog().unwrap();
        state.store.project_replication_backlog().unwrap();
    }
    let old_success = smallclaims::store::now_ms()
        - smallclaims::store::TRANSPORT_LINK_MAX_AGE_MS
        - smallclaims::store::TRANSPORT_LINK_CLOCK_SKEW_MS;
    // A legacy peer's immutable observation still syncs and must age by its source time.
    states[2]
        .store
        .append_legacy_claim(&ClaimInput {
            subject: "host/birch".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: serde_json::from_value(
                serde_json::json!({"status":"up","last_success_at":old_success}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    states[2]
        .store
        .append_claim(&ClaimInput {
            subject: "agent/fixture-seat".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/fixture-seat".into()),
            fields: serde_json::from_value(serde_json::json!({
                "runtime_id":"fern-seat", "incarnation_id":"fern:i1",
                "status":"running", "terminal":true
            }))
            .unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();

    states[2].store.append_claim(&ClaimInput {
        subject: "message/fixture-attachment".into(), kind: "message.sent".into(),
        actor: Some("person/avery".into()),
        fields: serde_json::from_value(serde_json::json!({
            "from":"person/avery", "to":"agent/fixture-seat", "content":"fixture", "status":"sent",
            "attachments":[{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "media_type":"image/png", "size":1, "origin":"host/fern"}]
        })).unwrap(),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();

    let sockets = std::array::from_fn::<_, 3, _>(|i| roots[i].path().join("st3.sock"));
    let mut servers = Vec::new();
    for i in 0..3 {
        let socket = sockets[i].clone();
        let app = crate::api::router(states[i].clone());
        servers.push(tokio::spawn(async move {
            crate::api::serve_unix(&socket, app).await.unwrap()
        }));
    }
    for socket in &sockets {
        for _ in 0..200 {
            if tokio::net::UnixStream::connect(socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists());
    }
    let auth = FleetAuth::test(fleet, &[7; 32]);
    let mut peers = Vec::new();
    // Fern has a daemon Unix socket but no inbound peer listener.
    for i in 0..2 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        peers.push(PeerConfig {
            name: names[i].into(),
            url: format!("http://{}", listener.local_addr().unwrap()),
        });
        let context = FleetContext::legacy(
            names
                .iter()
                .filter(|name| **name != names[i])
                .map(|name| (*name).into())
                .collect(),
        );
        let state = PeerState::new(
            MainBackend::new(sockets[i].clone()),
            names[i].into(),
            auth.clone(),
            context,
        );
        servers.push(tokio::spawn(async move {
            axum::serve(listener, peer_router(state, smalltalk_routes()))
                .await
                .unwrap()
        }));
    }
    let make_relay = |i: usize| {
        let secret = roots[i].path().join("fleet-secret");
        fs::write(&secret, [7_u8; 32]).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let mut relay = ClientRelay::from_config(&Config {
            node: names[i].into(),
            fleet_id: Some(fleet.into()),
            shared_secret_file: Some(secret),
            peers: peers
                .iter()
                .filter(|peer| peer.name != names[i])
                .cloned()
                .collect(),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
        .with_links(states[i].store.clone());
        relay.fabric = None;
        relay
    };
    let request = ClientReadRequest {
        authority_actor: "person/avery".into(),
        request: ClientReadOperation::AgentWorkspace {
            identity: "agent/fixture-seat".into(),
        },
        relay: None,
    };
    let replicate_fern = |i: usize| {
        let backend = MainBackend::new(sockets[i].clone());
        let exchange = states[2]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        async move {
            backend
                .receive("fern", fleet, &exchange, None)
                .await
                .unwrap()
        }
    };
    for i in 0..2 {
        replicate_fern(i).await;
        assert!(
            states[i]
                .store
                .latest_claim("host/birch", Some("transport.observed"))
                .unwrap()
                .is_some(),
            "the historical claim replicated"
        );
        assert!(states[i].store.transport_links().unwrap().is_empty());
        let relay = make_relay(i);
        assert!(
            Arc::ptr_eq(&relay.fleet_view(), &relay.fleet_view()),
            "per-item reachability reuses sealed membership"
        );
        assert!(!relay.reaches("host/fern"));
        assert!(
            relay.next_hops("fern", &[]).is_empty(),
            "expiry must not enable unseen-owner fallback"
        );
        let error = relay
            .read("host/fern", &request)
            .await
            .unwrap_err()
            .downcast::<ClientReadRejected>()
            .unwrap();
        assert_eq!(error.reason(), Some("dial-out-owner"));
        assert_eq!(error.details["attempts"], serde_json::json!([]));
        assert_eq!(error.status, 503);
        assert!(error.message.contains("has no inbound route"));
        let terminal_error = relay
            .raw_terminal(
                "host/fern",
                "person/avery",
                "terminal/fixture-seat",
                "fixture:i1",
                st3_client::RawTerminalMode::Attach,
            )
            .await
            .unwrap_err();
        assert_eq!(
            terminal_error
                .downcast::<ClientReadRejected>()
                .unwrap()
                .reason(),
            Some("dial-out-owner")
        );
        let mut gateway = states[i].clone();
        gateway.client_relay = Some(relay.clone());
        let response = crate::api::router(gateway.clone())
            .oneshot(
                Request::get("/v1/hosts/fern/agent-workspace?identity=agent%2Ffixture-seat")
                    .header("x-st3-person", "person/avery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let error: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(error["code"], "remote-unavailable");
        assert_eq!(error["details"]["reason"], "dial-out-owner");
        assert_eq!(error["details"]["hops"], 0);
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("has no inbound route")
        );
        for (method, path, body) in [
            (
                "GET",
                "/v1/client/conversations/agent%2Ffixture-seat/changes",
                "",
            ),
            (
                "GET",
                "/v1/client/blobs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?message=message%2Ffixture-attachment",
                "",
            ),
            (
                "GET",
                "/v1/client/terminals/terminal%2Fagent%2Ffixture-seat/screen",
                "",
            ),
            (
                "POST",
                "/v1/client/terminals/terminal%2Fagent%2Ffixture-seat/raw-attachments",
                r#"{"runtime_incarnation":"fern:i1","mode":"attach"}"#,
            ),
        ] {
            let response = crate::api::router(gateway.clone())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("x-st3-person", "person/avery")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let error: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {error}");
            assert_eq!(
                error["details"]["reason"], "dial-out-owner",
                "{path}: {error}"
            );
            assert_eq!(error["details"]["hops"], 0);
        }
        let listening = format!("host/{}", names[1 - i]);
        assert!(relay.reaches(&listening));
        relay.read(&listening, &request).await.unwrap();
        assert!(states[i].store.observes_transport_to(names[1 - i]).unwrap());
        assert!(!states[i].store.observes_transport_to("fern").unwrap());
    }
    // Even a fresh observation left by an older member cannot advertise a reverse route.
    states[2]
        .store
        .record_transport_observation("birch", "up", None, None)
        .unwrap();
    for (i, state) in states.iter().enumerate().take(2) {
        replicate_fern(i).await;
        assert!(state.store.transport_links().unwrap().is_empty());
        assert!(!make_relay(i).reaches("host/fern"));
    }
    // A real listening-peer receive still publishes fresh route evidence and deduplicates it.
    for i in 0..2 {
        let other = 1 - i;
        let backend = MainBackend::new(sockets[i].clone());
        let exchange = states[other]
            .store
            .export_replication_exchange(fleet, &ReplicationInventory::default())
            .unwrap();
        backend
            .receive(names[other], fleet, &exchange, None)
            .await
            .unwrap();
        assert!(
            states[i]
                .store
                .transport_links()
                .unwrap()
                .contains(&(names[i].into(), names[other].into()))
        );
        let before = states[i]
            .store
            .claims_for(
                &format!("host/{}", names[other]),
                Some("transport.observed"),
            )
            .unwrap()
            .len();
        backend
            .receive(names[other], fleet, &exchange, None)
            .await
            .unwrap();
        assert_eq!(
            states[i]
                .store
                .claims_for(
                    &format!("host/{}", names[other]),
                    Some("transport.observed")
                )
                .unwrap()
                .len(),
            before
        );
        let relay = make_relay(i);
        assert!(relay.reaches(&format!("host/{}", names[other])));
        relay
            .read(&format!("host/{}", names[other]), &request)
            .await
            .unwrap();
    }
    // The dial-out member can still read both listening owners in its outbound direction.
    let outbound = make_relay(2);
    for name in &names[..2] {
        assert!(outbound.reaches(&format!("host/{name}")));
        outbound
            .read(&format!("host/{name}"), &request)
            .await
            .unwrap();
    }
    // Published membership overrides a stale configured URL; correcting the owner mode
    // becomes visible when the shared five-second membership cache expires.
    let mut corrected = make_relay(0);
    corrected.peers.push(PeerConfig {
        name: "fern".into(),
        url: peers[1].url.clone(),
    });
    assert!(!corrected.reaches("host/fern"));
    states[2]
        .store
        .publish_fleet_endpoints("listening", &[], "test")
        .unwrap();
    replicate_fern(0).await;
    assert!(!corrected.reaches("host/fern"));
    corrected.membership.lock().unwrap().as_mut().unwrap().0 -= CLIENT_READ_LINKS_TTL;
    assert!(corrected.reaches("host/fern"));
    assert_eq!(
        corrected.next_hops("fern", &[]).first().unwrap().name,
        "fern"
    );
    for server in servers {
        server.abort();
    }
}
