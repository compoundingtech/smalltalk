struct ConversationHintFixture {
    _root: tempfile::TempDir,
    relay: ClientRelay,
    request: ClientReadRequest,
    owner_calls: Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
    servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for ConversationHintFixture {
    fn drop(&mut self) {
        for server in &self.servers {
            server.abort();
        }
    }
}

async fn conversation_hint_fixture(old_peer: bool, owner_status: Option<StatusCode>) -> ConversationHintFixture {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("native-home");
    let transcript = home.join(".codex/sessions/2026/09/24/hint.jsonl");
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, format!("{}\n{}\n",
        serde_json::json!({"type":"session_meta","timestamp":"2026-09-24T15:00:00Z","payload":{"id":"hint-native-id","cwd":root.path(),"source":"test"}}),
        serde_json::json!({"type":"response_item","timestamp":"2026-09-24T15:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Hint owner answer"}]}}),
    )).unwrap();
    let session = crate::external_sessions::discover_fresh(Some(&home), true)
        .unwrap().sessions.into_iter().find(|session| session.native_id == "hint-native-id").unwrap();
    let socket = root.path().join("st3.sock");
    let main = crate::api::AppState {
        store: Arc::new(Store::open_memory("owner").unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "owner".into(),
        state_dir: root.path().to_path_buf(),
        pty_root: root.path().join("pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec!["source".into()],
        client_relay: None,
        native_session_home: Some(home),
        planner_default: crate::model::PlannerSpec::default(),
    };
    let owner_calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let calls = owner_calls.clone();
    let app = crate::api::router(main).layer(axum::middleware::from_fn(
        move |request: Request<Body>, next: axum::middleware::Next| {
            let calls = calls.clone();
            async move {
                calls.lock().await.push((request.uri().path().to_owned(),
                    request.headers().get("x-st3-person").and_then(|value| value.to_str().ok()).unwrap_or("").to_owned()));
                if request.uri().path() == CLIENT_READ_OWNER_PATH
                    && let Some(status) = owner_status {
                    return status.into_response();
                }
                next.run(request).await
            }
        },
    ));
    let server_socket = socket.clone();
    let daemon = tokio::spawn(async move { crate::api::serve_unix(&server_socket, app).await.unwrap(); });
    for _ in 0..100 {
        if socket.exists() { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists());
    let peer = PeerState::new(MainBackend::new(socket), "owner".into(), FleetAuth::test("fleet-test", &[4; 32]), FleetContext::legacy(BTreeSet::from(["source".into(), "middle".into()])));
    // An old peer's ingress never interpreted unknown HTTP metadata. Strip it before
    // the unchanged legacy typed dispatch, while checking the original wire schema.
    let peer_app = peer_router(peer, smalltalk_routes()).layer(axum::middleware::from_fn(
        move |mut request: Request<Body>, next: axum::middleware::Next| async move {
            if old_peer {
                assert_eq!(request.headers().get(CLIENT_READ_SUBJECT_HINT_HEADER).unwrap(), "agent/hinted");
                request.headers_mut().remove(CLIENT_READ_SUBJECT_HINT_HEADER);
            }
            next.run(request).await
        },
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer_server = tokio::spawn(async move { axum::serve(listener, peer_app).await.unwrap(); });
    let secret = root.path().join("secret");
    fs::write(&secret, [4_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let relay = ClientRelay::from_config(&Config {
        node: "source".into(), fleet_id: Some("fleet-test".into()), shared_secret_file: Some(secret),
        peers: vec![PeerConfig { name: "owner".into(), url: format!("http://{address}") }],
        ..Default::default()
    }).unwrap().unwrap();
    ConversationHintFixture {
        _root: root, relay,
        request: ClientReadRequest { authority_actor: "person/test".into(), relay: None,
            request: ClientReadOperation::Timeline { session_id: session.id, limit: 20, cursor: None } },
        owner_calls, servers: vec![daemon, peer_server],
    }
}

fn assert_hint_timeline(value: &Value) {
    assert!(value["items"].as_array().unwrap().iter().any(|item| item["body"]["text"] == "Hint owner answer"));
}

#[tokio::test]
async fn conversation_subject_hint_new_gateway_old_peer_preserves_original_json() {
    let fixture = conversation_hint_fixture(true, None).await;
    let original = serde_json::to_value(&fixture.request).unwrap();
    assert!(original.get("subject_hint").is_none());
    let value = fixture.relay.read_with_subject_hint("host/owner", &fixture.request, Some("agent/hinted")).await.unwrap();
    assert_hint_timeline(&value);
    assert_eq!(serde_json::to_value(&fixture.request).unwrap(), original);
    assert!(!fixture.owner_calls.lock().await.iter().any(|(path, _)| path == CLIENT_READ_OWNER_PATH));
}

#[tokio::test]
async fn conversation_subject_hint_old_gateway_new_owner_uses_legacy_dispatch() {
    let fixture = conversation_hint_fixture(false, None).await;
    assert_hint_timeline(&fixture.relay.read("host/owner", &fixture.request).await.unwrap());
    assert!(!fixture.owner_calls.lock().await.iter().any(|(path, _)| path == CLIENT_READ_OWNER_PATH));
}

#[tokio::test]
async fn conversation_subject_hint_owner_route_preserves_authority() {
    let fixture = conversation_hint_fixture(false, None).await;
    // This deliberately mismatches the native session: the core must fall back to
    // ordinary discovery rather than grant access to an arbitrary hinted subject.
    assert_hint_timeline(&fixture.relay.read_with_subject_hint("host/owner", &fixture.request, Some("agent/hinted")).await.unwrap());
    assert!(fixture.owner_calls.lock().await.iter().any(|(path, actor)| path == CLIENT_READ_OWNER_PATH && actor == "person/test"));
}

#[tokio::test]
async fn conversation_subject_hint_preserves_legacy_actor_syntax() {
    let mut fixture = conversation_hint_fixture(false, None).await;
    fixture.request.authority_actor = "agent/legacy:actor".into();
    assert_hint_timeline(&fixture.relay.read_with_subject_hint(
        "host/owner", &fixture.request, Some("agent/hinted"),
    ).await.unwrap());
    let calls = fixture.owner_calls.lock().await;
    assert!(calls.iter().all(|(path, actor)| path != CLIENT_READ_OWNER_PATH
        && actor == "agent/legacy:actor"));
}

#[tokio::test]
async fn conversation_subject_hint_old_daemon_404_falls_back_to_legacy_dispatch() {
    let fixture = conversation_hint_fixture(false, Some(StatusCode::NOT_FOUND)).await;
    assert_hint_timeline(&fixture.relay.read_with_subject_hint("host/owner", &fixture.request, Some("agent/hinted")).await.unwrap());
    let calls = fixture.owner_calls.lock().await;
    assert!(calls.iter().any(|(path, _)| path == CLIENT_READ_OWNER_PATH));
    assert!(calls.iter().any(|(path, _)| path != CLIENT_READ_OWNER_PATH));
}

#[tokio::test]
async fn conversation_subject_hint_authorization_failure_never_falls_back() {
    let fixture = conversation_hint_fixture(false, Some(StatusCode::FORBIDDEN)).await;
    assert!(fixture.relay.read_with_subject_hint("host/owner", &fixture.request, Some("agent/hinted")).await.is_err());
    let calls = fixture.owner_calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], (CLIENT_READ_OWNER_PATH.to_owned(), "person/test".to_owned()));
}

#[tokio::test]
async fn conversation_subject_hint_preserves_operation_bounds() {
    let mut fixture = conversation_hint_fixture(false, None).await;
    for operation in [
        ClientReadOperation::Timeline { session_id: "session/fixture".into(), limit: 0, cursor: None },
        ClientReadOperation::ConversationChanges { session_id: "session/fixture".into(),
            after: None, wait_ms: CLIENT_READ_MAX_WAIT_MS + 1 },
    ] {
        fixture.request.request = operation;
        for hint in [None, Some("agent/hinted")] {
            assert!(fixture.relay.read_with_subject_hint(
                "host/owner", &fixture.request, hint,
            ).await.is_err());
        }
    }
    assert!(fixture.owner_calls.lock().await.is_empty());
}

#[test]
fn conversation_subject_hint_metadata_is_bounded_and_concrete() {
    assert_eq!(conversation_subject_hint("agent/hinted"), Some("agent/hinted"));
    for invalid in ["person/test", "agent/", "agent/a b", "agent/a?b", "agent//a"] {
        assert_eq!(conversation_subject_hint(invalid), None);
    }
    let bound = format!("agent/{}", "a".repeat(506));
    assert!(conversation_subject_hint(&bound).is_some());
    assert!(conversation_subject_hint(&(bound + "a")).is_none());
}

#[tokio::test]
async fn conversation_subject_hint_multihop_preserves_query_and_wire_body() {
    let mut fixture = conversation_hint_fixture(false, None).await;
    let mut onward = fixture.relay.clone();
    onward.node = "middle".into();
    let socket = fixture._root.path().join("middle.sock");
    let state = crate::api::AppState {
        store: Arc::new(Store::open_memory("middle").unwrap()),
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "middle".into(),
        state_dir: fixture._root.path().to_path_buf(),
        pty_root: fixture._root.path().join("middle-pty"),
        pty_binary: PathBuf::from("pty"),
        fleet_id: None,
        configured_peers: vec!["source".into(), "owner".into()],
        client_relay: Some(onward),
        native_session_home: None,
        planner_default: crate::model::PlannerSpec::default(),
    };
    let app = crate::api::router(state).layer(axum::middleware::from_fn(
        |request: Request<Body>, next: axum::middleware::Next| async move {
            assert_eq!(request.uri().path(), CLIENT_READ_FORWARD_PATH);
            assert_eq!(request.uri().query(), Some("conversation_subject_hint=agent%2Fhinted"));
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, MAX_CLIENT_READ_BYTES).await.unwrap();
            // deny_unknown_fields makes any attempt to stuff hint metadata in JSON fail.
            let read: ClientReadRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(read.authority_actor, "person/test");
            let route = read.relay.unwrap();
            assert_eq!(route.target, "host/owner");
            assert_eq!(route.path, vec!["source"]);
            next.run(Request::from_parts(parts, Body::from(bytes))).await
        },
    ));
    let served_socket = socket.clone();
    fixture.servers.push(tokio::spawn(async move {
        crate::api::serve_unix(&served_socket, app).await.unwrap();
    }));
    for _ in 0..100 {
        if socket.exists() { break; }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(socket.exists());
    let middle = PeerState::new(MainBackend::new(socket), "middle".into(),
        FleetAuth::test("fleet-test", &[4; 32]),
        FleetContext::legacy(BTreeSet::from(["source".into()])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    fixture.servers.push(tokio::spawn(async move {
        axum::serve(listener, peer_router(middle, smalltalk_routes())).await.unwrap();
    }));
    fixture.relay.peers = vec![PeerConfig { name: "middle".into(), url: format!("http://{address}") }];
    assert_hint_timeline(&fixture.relay.read_with_subject_hint(
        "host/owner", &fixture.request, Some("agent/hinted")).await.unwrap());
    assert!(fixture.owner_calls.lock().await.iter().any(|(path, actor)|
        path == CLIENT_READ_OWNER_PATH && actor == "person/test"));
}

#[tokio::test]
async fn conversation_subject_hint_cannot_replace_fleet_authentication() {
    let fixture = conversation_hint_fixture(false, None).await;
    let response = reqwest::Client::new()
        .post(format!("{}{}", fixture.relay.peers[0].url, CLIENT_READ_PATH))
        .header(CLIENT_READ_SUBJECT_HINT_HEADER, "agent/hinted")
        .json(&fixture.request)
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(fixture.owner_calls.lock().await.is_empty());
}

#[tokio::test]
async fn conversation_subject_hint_malformed_header_is_ignored() {
    let fixture = conversation_hint_fixture(false, None).await;
    let body = serde_json::to_vec(&fixture.request).unwrap();
    let headers = fixture.relay.auth.request_headers_for(CLIENT_READ_PATH, "source", &body).unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}{}", fixture.relay.peers[0].url, CLIENT_READ_PATH))
        .headers(headers)
        .header(CLIENT_READ_SUBJECT_HINT_HEADER, "person/test")
        .body(body)
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: ApiResponse<Value> = response.json().await.unwrap();
    assert_hint_timeline(&response.value);
    assert!(!fixture.owner_calls.lock().await.iter().any(|(path, _)| path == CLIENT_READ_OWNER_PATH));
}
