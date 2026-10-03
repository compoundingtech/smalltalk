mod gateway_tests {
    use super::*;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

    fn paired_device(state: &AppState, credential: &str, suffix: &str) {
        paired_device_scoped(state, credential, suffix, &["read.projections", "terminal.read"]);
    }

    fn paired_device_scoped(state: &AppState, credential: &str, suffix: &str, scopes: &[&str]) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: format!("custom/client/pairing-{suffix}"),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([
                    (
                        "credential_hash".into(),
                        json!(credential_digest(credential)),
                    ),
                    (
                        "session_actor".into(),
                        json!(format!("person/alex/session/{suffix}")),
                    ),
                    ("person_id".into(), json!("person/alex")),
                    ("scopes".into(), json!(scopes)),
                    (
                        "expires_at_unix_ms".into(),
                        json!(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    #[test]
    fn cookies_cannot_authenticate_and_native_bearers_keep_unix_authority() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        paired_device(&state, "bearer-secret", "bearer");
        let request = |bearer: Option<&str>| {
            let mut request = Request::builder()
                .uri("/v1/client/agents")
                .header(axum::http::header::COOKIE, "st3_device=bearer-secret");
            if let Some(bearer) = bearer {
                request = request.header(AUTHORIZATION, bearer);
            }
            request.body(Body::empty()).unwrap()
        };
        assert!(authenticate(&state, &request(None), "fabric-loopback").is_err());
        assert_eq!(
            authenticate(
                &state,
                &request(Some("Bearer bearer-secret")),
                "fabric-loopback"
            )
            .unwrap()
            .actor,
            "person/alex/session/bearer"
        );
        for bearer in ["Bearer unknown", "Basic bearer-secret", "Bearer "] {
            assert!(authenticate(&state, &request(Some(bearer)), "fabric-loopback").is_err());
        }
        assert_eq!(
            authenticate(&state, &request(None), "unix").unwrap().actor,
            "client/local/read-only"
        );
    }

    #[tokio::test]
    async fn pairing_returns_native_bearer_without_cookie_and_rejects_reuse() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let Json(challenge) = pairing_begin(
            State(state.clone()),
            Extension(ClientSession::local(Some("person/alex")).unwrap()),
            Json(PairingBegin {
                api_version: CLIENT_API_VERSION.into(),
                device_name: "Browser".into(),
                person_id: "person/alex".into(),
                full_control: None,
                scopes: None,
            }),
        )
        .await
        .unwrap();
        let uri = format!(
            "/v1/client/pairings/{}/complete",
            challenge["pairing_id"]
                .as_str()
                .unwrap()
                .trim_start_matches("pairing/")
        );
        let app = crate::api::fabric_router(state.clone());
        let complete = |code: &str| {
            let body = json!({"api_version": CLIENT_API_VERSION, "code": code, "device_public_key": "browser-public-key-01234567890123456789"});
            Request::builder()
                .method("POST")
                .uri(&uri)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let refused = app.clone().oneshot(complete("wrong")).await.unwrap();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(
            !refused
                .headers()
                .contains_key(axum::http::header::SET_COOKIE)
        );
        let response = app
            .clone()
            .oneshot(complete(challenge["code"].as_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::SET_COOKIE)
        );
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                .await
                .unwrap(),
        )
        .unwrap();
        let credential = body["value"]["credential"].as_str().unwrap();
        let authenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/client/capabilities")
                    .header(AUTHORIZATION, format!("Bearer {credential}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);
        let reused = app
            .oneshot(complete(challenge["code"].as_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(reused.status(), StatusCode::FORBIDDEN);
        assert!(
            !reused
                .headers()
                .contains_key(axum::http::header::SET_COOKIE)
        );
    }

    #[tokio::test]
    async fn websocket_authorization_precedes_malformed_fallback_without_downgrade() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        paired_device(&state, "native-secret", "native");
        paired_device(&state, "fallback-secret", "fallback");
        let (address, server) = serve(crate::api::fabric_router(state)).await;
        for fallback in [
            BEARER_PROTOCOL_PREFIX.to_owned(),
            format!("{BEARER_PROTOCOL_PREFIX}unknown, {BEARER_PROTOCOL_PREFIX}fallback-secret"),
            format!("{BEARER_PROTOCOL_PREFIX}fallback-secret"),
        ] {
            for authorization in ["Bearer native-secret", "Bearer unknown", "Basic native-secret"] {
                let mut request = format!("ws://{address}/v1/client/collections/stream")
                    .into_client_request()
                    .unwrap();
                request.headers_mut().insert(
                    SEC_WEBSOCKET_PROTOCOL,
                    format!("{COLLECTION_SUBPROTOCOL}, {fallback}")
                        .parse()
                        .unwrap(),
                );
                request
                    .headers_mut()
                    .insert(AUTHORIZATION, authorization.parse().unwrap());
                let result = tokio_tungstenite::connect_async(request).await;
                if authorization == "Bearer native-secret" {
                    let (mut socket, response) = result.unwrap();
                    assert_eq!(
                        response.headers()[SEC_WEBSOCKET_PROTOCOL],
                        COLLECTION_SUBPROTOCOL
                    );
                    socket
                        .send(Message::Text(
                            json!({"kind": "subscribe", "id": "native-agents", "collection": "agents"})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                    let snapshot = next_json(&mut socket).await;
                    assert_eq!(snapshot["kind"], "snapshot");
                    assert_eq!(snapshot["id"], "native-agents");
                    socket.close(None).await.unwrap();
                } else {
                    assert!(matches!(
                        result.unwrap_err(),
                        tokio_tungstenite::tungstenite::Error::Http(response)
                            if response.status() == StatusCode::FORBIDDEN
                                && !response.headers().contains_key(SEC_WEBSOCKET_PROTOCOL)
                    ));
                }
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn websocket_bearer_authenticates_before_upgrade_and_is_never_echoed() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        paired_device(&state, "browser-secret", "browser");
        let (address, server) = serve(crate::api::fabric_router(state.clone())).await;
        let request = |protocols: &str, authorization: Option<&str>| {
            let mut request = format!("ws://{address}/v1/client/collections/stream")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, protocols.parse().unwrap());
            if let Some(value) = authorization {
                request
                    .headers_mut()
                    .insert(AUTHORIZATION, value.parse().unwrap());
            }
            request
        };
        let protocols = format!("{COLLECTION_SUBPROTOCOL}, {BEARER_PROTOCOL_PREFIX}browser-secret");
        let (mut socket, response) = tokio_tungstenite::connect_async(request(&protocols, None))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[SEC_WEBSOCKET_PROTOCOL],
            COLLECTION_SUBPROTOCOL
        );
        socket
            .send(Message::Text(
                json!({
                    "kind": "subscribe", "id": "browser-agents", "collection": "agents"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let snapshot = next_json(&mut socket).await;
        assert_eq!(snapshot["kind"], "snapshot");
        assert_eq!(snapshot["id"], "browser-agents");
        socket.close(None).await.unwrap();

        for (offered, authorization) in [
            (COLLECTION_SUBPROTOCOL.to_owned(), None),
            (
                format!("{COLLECTION_SUBPROTOCOL}, {BEARER_PROTOCOL_PREFIX}unknown"),
                None,
            ),
            (
                format!("{protocols}, {BEARER_PROTOCOL_PREFIX}browser-secret"),
                None,
            ),
            (
                format!("{COLLECTION_SUBPROTOCOL}, {BEARER_PROTOCOL_PREFIX}"),
                None,
            ),
            (protocols.clone(), Some("Bearer unknown")),
        ] {
            let error = tokio_tungstenite::connect_async(request(&offered, authorization))
                .await
                .unwrap_err();
            match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    assert_eq!(response.status(), StatusCode::FORBIDDEN);
                    assert!(!response.headers().contains_key(SEC_WEBSOCKET_PROTOCOL));
                }
                other => panic!("expected refused handshake, got {other}"),
            }
        }

        // The bearer carrier is accepted only at a WebSocket upgrade, never on HTTP reads.
        let read = Request::builder()
            .uri("/v1/client/agents")
            .header(SEC_WEBSOCKET_PROTOCOL, &protocols)
            .body(Body::empty())
            .unwrap();
        assert!(authenticate(&state, &read, "fabric-loopback").is_err());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/pairing-browser".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let error = tokio_tungstenite::connect_async(request(&protocols, None))
            .await
            .unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == StatusCode::FORBIDDEN)
        );
        server.abort();
    }

    async fn serve(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, task)
    }

    type TestSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect(address: std::net::SocketAddr, path: &str, protocols: &str) -> TestSocket {
        connect_as(address, path, protocols, "viewer-secret").await
    }

    async fn connect_as(
        address: std::net::SocketAddr,
        path: &str,
        protocols: &str,
        credential: &str,
    ) -> TestSocket {
        let mut request = format!("ws://{address}{path}")
            .into_client_request()
            .unwrap();
        // Model browser WebSocket: no Authorization header, only offered protocols.
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            format!("{protocols}, {BEARER_PROTOCOL_PREFIX}{credential}")
                .parse()
                .unwrap(),
        );
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    async fn next_json(socket: &mut TestSocket) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match socket.next().await.unwrap().unwrap() {
                    Message::Text(value) => return serde_json::from_str(&value).unwrap(),
                    Message::Binary(value) => return serde_json::from_slice(&value).unwrap(),
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }

    fn viewer_attachment(state: &AppState) -> (ClientSession, Value) {
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/terminal-owner".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/terminal-owner".into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), json!("terminal-runtime")),
                    ("incarnation_id".into(), json!("terminal-runtime:i1")),
                    ("status".into(), json!("running")),
                    ("terminal".into(), json!(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        paired_device(state, "viewer-secret", "viewer");
        let auth = Request::builder()
            .uri("/v1/client/agents")
            .header(AUTHORIZATION, "Bearer viewer-secret")
            .body(Body::empty())
            .unwrap();
        let session = authenticate(state, &auth, "fabric-loopback").unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/viewer".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "viewer-attachment".into(),
            fence: Fence {
                runtime_incarnation: Some("terminal-runtime:i1".into()),
                ..Fence::default()
            },
            parameters: json!({"target_id": "terminal/agent/terminal-owner"}),
        };
        let attachment =
            create_terminal_attachment(state, &session, &request, "viewer-digest").unwrap();
        (session, attachment)
    }

    #[test]
    fn projected_lease_survives_last_viewer_for_every_transport() {
        for transport in ["unix", "fabric-loopback", "tailscale"] {
            let root = tempfile::tempdir().unwrap();
            let state = test_state(root.path());
            let (mut session, attachment) = viewer_attachment(&state);
            session.transport = transport;
            let open = || prepare_terminal_follow(
                &state,
                &session,
                "agent/terminal-owner",
                Some("terminal-runtime:i1"),
                attachment["stream_capability"].as_str(),
            ).unwrap();
            let first = open();
            let second = open();
            drop(first);
            drop(second);
            assert_eq!(
                terminal_attachment_response(&state, &session, attachment["attachment_id"].as_str().unwrap())
                    .unwrap()["state"],
                "available",
                "{transport}",
            );
            // Reopening the same capability is the client-visible reconnect contract.
            drop(open());
            let request = ActionRequest {
                api_version: CLIENT_API_VERSION.into(),
                id: "action/detach".into(),
                action_type: "terminal.detach".into(),
                idempotency_key: "detach".into(),
                fence: Fence {
                    runtime_incarnation: Some("terminal-runtime:i1".into()),
                    ..Fence::default()
                },
                parameters: json!({"target_id": attachment["attachment_id"]}),
            };
            detach_terminal_attachment(&state, &session, &request).unwrap();
            assert_eq!(
                prepare_terminal_follow(&state, &session, "agent/terminal-owner",
                    Some("terminal-runtime:i1"), attachment["stream_capability"].as_str())
                    .err().unwrap().code,
                "forbidden",
            );
        }
    }

    #[tokio::test]
    async fn raw_capability_stays_single_use_and_detaches_when_its_viewer_drops() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let (session, _) = viewer_attachment(&state);
        let Json(attachment) = raw_terminal::attachment(
            State(state.clone()), Extension(session.clone()),
            AxumPath("agent/terminal-owner".into()),
            Json(serde_json::from_value(json!({
                "runtime_incarnation": "terminal-runtime:i1", "mode": "peek",
            })).unwrap()),
        ).await.unwrap();
        let consume = || consume_terminal_attachment_mode(
            &state, &session, "terminal/agent/terminal-owner", "terminal-runtime:i1",
            attachment["stream_capability"].as_str(), Some("peek"),
        );
        let viewer = consume().unwrap();
        let subject = viewer.subject.clone();
        assert_eq!(consume().unwrap_err().code, "forbidden");
        assert_eq!(state.store.claims_for(&subject, None).unwrap().last().unwrap().kind,
            "custom.client.terminal-consumed");
        drop(viewer);
        assert_eq!(state.store.claims_for(&subject, None).unwrap().last().unwrap().kind,
            "custom.client.terminal-detached");
        assert_eq!(consume().unwrap_err().code, "forbidden");
    }

    #[tokio::test]
    async fn shared_projected_lease_streams_and_reconnects_after_all_viewers_close() {
        use pty_core::protocol::{MessageType, encode_data, encode_geometry, encode_packet};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let (session, attachment) = viewer_attachment(&state);
        fs::create_dir_all(&state.pty_root).unwrap();
        let listener = tokio::net::UnixListener::bind(
            state.pty_root.join("terminal-runtime.sock"),
        ).unwrap();
        let (address, server) = serve(crate::api::fabric_router(state.clone())).await;
        let protocols = format!(
            "{TERMINAL_SUBPROTOCOL}, {TERMINAL_CAPABILITY_PROTOCOL_PREFIX}{}",
            attachment["stream_capability"].as_str().unwrap(),
        );
        let path = "/v1/client/terminals/agent%2Fterminal-owner/stream?incarnation=terminal-runtime%3Ai1";
        let mut first = connect(address, path, &protocols).await;
        let mut second = connect(address, path, &protocols).await;
        let (mut pty, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await.unwrap().unwrap();
        let mut request = [0_u8; 6];
        pty.read_exact(&mut request).await.unwrap();
        pty.write_all(&encode_geometry(2, 40)).await.unwrap();
        pty.write_all(&encode_packet(MessageType::Screen, b"before")).await.unwrap();
        assert_eq!(next_json(&mut first).await["value"]["lines"][0]["text"], "before");
        assert_eq!(next_json(&mut second).await["value"]["lines"][0]["text"], "before");

        first.close(None).await.unwrap();
        assert_eq!(
            terminal_attachment_response(&state, &session, attachment["attachment_id"].as_str().unwrap())
                .unwrap()["state"],
            "available",
        );
        pty.write_all(&encode_data(b" after")).await.unwrap();
        assert_eq!(next_json(&mut second).await["value"]["lines"][0]["text"], "before after");
        second.close(None).await.unwrap();
        let mut reconnected = connect(address, path, &protocols).await;
        assert_eq!(next_json(&mut reconnected).await["value"]["lines"][0]["text"], "before after");
        pty.write_all(&encode_data(b" reconnect")).await.unwrap();
        assert_eq!(next_json(&mut reconnected).await["value"]["lines"][0]["text"], "before after reconnect");
        reconnected.close(None).await.unwrap();
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn replacing_terminal_subscription_keeps_its_projected_lease() {
        use pty_core::protocol::{MessageType, encode_geometry, encode_packet};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let (session, attachment) = viewer_attachment(&state);
        fs::create_dir_all(&state.pty_root).unwrap();
        let listener = tokio::net::UnixListener::bind(
            state.pty_root.join("terminal-runtime.sock"),
        ).unwrap();
        let (address, server) = serve(crate::api::fabric_router(state.clone())).await;
        let mut socket = connect(address, "/v1/client/collections/stream", COLLECTION_SUBPROTOCOL).await;
        let subscribe = json!({
            "kind":"subscribe", "id":"viewer", "collection":"terminal",
            "terminal":"terminal/agent/terminal-owner", "incarnation":"terminal-runtime:i1",
            "capability":attachment["stream_capability"],
        }).to_string();
        socket.send(Message::Text(subscribe.clone().into())).await.unwrap();
        let (mut pty, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await.unwrap().unwrap();
        let mut request = [0_u8; 6];
        pty.read_exact(&mut request).await.unwrap();
        pty.write_all(&encode_geometry(2, 20)).await.unwrap();
        pty.write_all(&encode_packet(MessageType::Screen, b"held")).await.unwrap();
        assert_eq!(next_json(&mut socket).await["value"]["lines"][0]["text"], "held");
        for _ in 0..10 {
            socket.send(Message::Text(subscribe.clone().into())).await.unwrap();
            let frame = next_json(&mut socket).await;
            assert_eq!(frame["kind"], "screen", "{frame}");
            assert_eq!(frame["value"]["lines"][0]["text"], "held");
        }
        assert_eq!(
            terminal_attachment_response(&state, &session, attachment["attachment_id"].as_str().unwrap())
                .unwrap()["state"],
            "available",
        );
        socket.close(None).await.unwrap();
        assert_eq!(
            terminal_attachment_response(&state, &session, attachment["attachment_id"].as_str().unwrap())
                .unwrap()["state"],
            "available",
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn collection_websocket_records_query_parent_and_subscription_override() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        paired_device(&state, "viewer-secret", "viewer");
        let (exports, mut received) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let collector = Router::new().route(
            "/v1/traces",
            post(move |Json(body): Json<Value>| {
                let exports = exports.clone();
                async move {
                    exports.send(body).unwrap();
                    StatusCode::OK
                }
            }),
        );
        let (collector_address, collector_server) = serve(collector).await;
        let web = super::super::super::client_web::ClientWeb::new(
            super::super::super::client_web::ClientWebConfig {
                static_dir: None,
                mount: "/app".into(),
                otlp_endpoint: Some(format!("http://{collector_address}")),
            },
            &state,
        )
        .unwrap();
        let (address, server) = serve(crate::api::fabric_router_with_web(state, web)).await;
        let query_parent = "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01";
        let override_parent = "00-fedcba9876543210fedcba9876543210-fedcba9876543210-01";
        let mut socket = connect(
            address,
            &format!("/v1/client/collections/stream?traceparent={query_parent}"),
            COLLECTION_SUBPROTOCOL,
        )
        .await;
        for (id, traceparent) in [("override", Some(override_parent)), ("fallback", None)] {
            socket.send(Message::Text(json!({"kind":"subscribe", "id":id, "collection":"agents", "traceparent":traceparent}).to_string().into())).await.unwrap();
            assert_eq!(next_json(&mut socket).await["kind"], "snapshot");
        }
        let mut spans = Vec::new();
        for _ in 0..3 {
            let export = tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
            spans.push(export["resourceSpans"][0]["scopeSpans"][0]["spans"][0].clone());
        }
        let upgrade = spans
            .iter()
            .find(|span| span["name"] == "st3.client.collections.upgrade")
            .unwrap();
        assert_eq!(upgrade["traceId"], "0123456789abcdef0123456789abcdef");
        assert_eq!(upgrade["parentSpanId"], "0123456789abcdef");
        let subscriptions = spans
            .iter()
            .filter(|span| span["name"] == "st3.client.collections.subscribe")
            .collect::<Vec<_>>();
        assert!(
            subscriptions
                .iter()
                .any(|span| span["traceId"] == "fedcba9876543210fedcba9876543210"
                    && span["parentSpanId"] == "fedcba9876543210")
        );
        assert!(
            subscriptions
                .iter()
                .any(|span| span["traceId"] == "0123456789abcdef0123456789abcdef"
                    && span["parentSpanId"] == "0123456789abcdef")
        );
        socket.close(None).await.unwrap();
        server.abort();
        collector_server.abort();
        let _ = server.await;
        let _ = collector_server.await;
    }

    #[tokio::test]
    async fn collection_limit_is_configurable_and_replacement_does_not_consume_a_slot() {
        for limit in [2, super::super::ClientSubscriptionLimit::default().0] {
            let root = tempfile::tempdir().unwrap();
            let state = test_state(root.path());
            paired_device(&state, "viewer-secret", "limit");
            let app = super::super::fabric_router(state)
                .layer(Extension(super::super::ClientSubscriptionLimit(limit)));
            let (address, server) = serve(app).await;
            let mut socket = connect(
                address,
                "/v1/client/collections/stream",
                COLLECTION_SUBPROTOCOL,
            )
            .await;
            for slot in 0..limit {
                socket
                    .send(Message::Text(
                        json!({
                            "kind":"subscribe", "id":format!("slot-{slot}"), "collection":"agents"
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
                let frame = next_json(&mut socket).await;
                assert_eq!(frame["kind"], "snapshot", "{frame}");
                assert_eq!(frame["id"], format!("slot-{slot}"));
            }
            socket
                .send(Message::Text(
                    json!({
                        "kind":"subscribe", "id":"overflow", "collection":"agents"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let frame = next_json(&mut socket).await;
            assert_eq!(frame["code"], "subscription-limit");
            socket
                .send(Message::Text(
                    json!({
                        "kind":"subscribe", "id":"slot-0", "collection":"agents"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            assert_eq!(next_json(&mut socket).await["kind"], "snapshot");
            socket
                .send(Message::Text(
                    json!({"kind":"unsubscribe", "id":"slot-0"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "kind":"subscribe", "id":"overflow", "collection":"agents"
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            assert_eq!(next_json(&mut socket).await["kind"], "snapshot");
            socket.close(None).await.unwrap();
            server.abort();
            let _ = server.await;
        }
    }

    async fn send_json(socket: &mut TestSocket, value: Value) {
        socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    /// The next input frame, past screens and the terminal errors that end follows.
    async fn next_input(socket: &mut TestSocket) -> Value {
        loop {
            let frame = next_json(socket).await;
            if frame["kind"] != "screen" && frame["collection"] != "terminal" {
                return frame;
            }
        }
    }

    /// One real PTY driven through input sessions: a repeat is acknowledged without a write,
    /// and a gap, a detach, an incarnation change or a revoked pairing closes input before its
    /// batch is written. Only a `terminal.control` device opens input at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_input_writes_each_batch_once_in_order_and_closes_without_writing() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some(pty) = std::env::split_paths(&path)
            .map(|dir| dir.join("pty"))
            .find(|p| p.is_file())
        else {
            assert!(std::env::var_os("CI").is_none(), "CI must provide pty");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let mut state = test_state(root.path());
        state.pty_binary = pty.clone();
        let runtime =
            st_runtime::PtyRuntime::new(state.pty_root.clone()).with_binary(pty.to_string_lossy());
        struct Cleanup(st_runtime::PtyRuntime);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.stop("input-test");
                let _ = self.0.remove("input-test");
            }
        }
        let _cleanup = Cleanup(runtime.clone());
        let pty_root = state.pty_root.clone();
        let output = tokio::task::spawn_blocking(move || std::process::Command::new(pty)
            .env("PTY_ROOT", pty_root)
            .args(["run", "-d", "--force", "--id", "input-test", "--tag", "keep=true", "--", "/bin/sh", "-c", "stty -echo; printf ready; while IFS= read -r line; do printf '\\r\\naccepted:%s' \"$line\"; done"])
            .output().unwrap()).await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let live = runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|live| live.name == "input-test")
            .unwrap();
        let incarnation = format!("{}:{}", live.pid.unwrap(), live.created_at.unwrap());
        let observe = |incarnation: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "agent/input-test".into(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("runtime_id".into(), json!("input-test")),
                        ("incarnation_id".into(), json!(incarnation)),
                        ("status".into(), json!("running")),
                        ("terminal".into(), json!(true)),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            signal_changed(&state);
        };
        observe(&incarnation);
        paired_device_scoped(
            &state,
            "control-secret",
            "control",
            &["read.projections", "terminal.read", "terminal.control"],
        );
        paired_device(&state, "viewer-secret", "viewer");
        let auth = Request::builder()
            .uri("/v1/client/agents")
            .header(AUTHORIZATION, "Bearer control-secret")
            .body(Body::empty())
            .unwrap();
        let control = authenticate(&state, &auth, "fabric-loopback").unwrap();
        let request = |action_type: &str, key: &str, parameters: Value| ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: format!("action/{key}"),
            action_type: action_type.into(),
            idempotency_key: key.into(),
            fence: Fence {
                runtime_incarnation: Some(incarnation.clone()),
                ..Fence::default()
            },
            parameters,
        };
        let attach = |key: &str| {
            let target = json!({"target_id": "terminal/agent/input-test"});
            create_terminal_attachment(&state, &control, &request("terminal.attach", key, target), key)
                .unwrap()
        };
        let (address, server) = serve(crate::api::fabric_router(state.clone())).await;
        let mut socket = connect_as(
            address,
            "/v1/client/collections/stream",
            COLLECTION_SUBPROTOCOL,
            "control-secret",
        )
        .await;
        let follow = async |socket: &mut TestSocket, id: &str, attachment: &Value| {
            send_json(socket, json!({"kind":"subscribe", "id":id, "collection":"terminal", "terminal":"terminal/agent/input-test", "incarnation":incarnation, "capability":attachment["stream_capability"]})).await;
            while next_json(socket).await["kind"] != "screen" {}
        };
        let open = async |socket: &mut TestSocket, id: &str, follow: &str| {
            send_json(socket, json!({"kind":"input-open", "id":id, "follow":follow})).await;
            assert_eq!(
                next_input(socket).await,
                json!({"kind":"input-opened", "id":id, "follow":follow, "next_seq":0})
            );
        };
        let input = async |socket: &mut TestSocket, id: &str, seq: u64, data: Value| {
            send_json(socket, json!({"kind":"input", "id":id, "seq":seq, "data":data})).await;
            next_input(socket).await
        };
        let ack = |id: &str, seq: u64| json!({"kind":"input-ack", "id":id, "seq":seq});
        let text = |line: &str| json!({"text": format!("{line}\r")});

        let first = attach("input-first");
        follow(&mut socket, "term", &first).await;
        open(&mut socket, "in", "term").await;
        assert_eq!(input(&mut socket, "in", 0, text("one")).await, ack("in", 0));
        let encoded = base64::engine::general_purpose::STANDARD.encode("two\r");
        let bytes = json!({"bytes_b64": encoded});
        assert_eq!(input(&mut socket, "in", 1, bytes).await, ack("in", 1));
        // A repeat of an acknowledged batch is acknowledged again and never written.
        assert_eq!(input(&mut socket, "in", 0, text("repeat")).await, ack("in", 0));
        let gap = input(&mut socket, "in", 3, text("gapped")).await;
        assert_eq!((gap["kind"].as_str(), gap["reason"].as_str()), (Some("input-closed"), Some("gap")));
        // A batch still on its way after the close is dropped without a frame.
        send_json(&mut socket, json!({"kind":"input", "id":"in", "seq":2, "data":text("late")})).await;
        open(&mut socket, "in", "term").await;
        assert_eq!(input(&mut socket, "in", 0, text("kept")).await, ack("in", 0));
        let target = json!({"target_id": first["attachment_id"]});
        detach_terminal_attachment(&state, &control, &request("terminal.detach", "detach-first", target))
            .unwrap();
        let detached = input(&mut socket, "in", 1, text("detached")).await;
        assert_eq!(detached["reason"], "detached");

        let second = attach("input-second");
        follow(&mut socket, "term2", &second).await;
        open(&mut socket, "in2", "term2").await;
        observe("input-test:replacement");
        let changed = input(&mut socket, "in2", 0, text("replaced")).await;
        assert_eq!((changed["id"].as_str(), changed["reason"].as_str()), (Some("in2"), Some("incarnation-changed")));

        observe(&incarnation);
        let third = attach("input-third");
        follow(&mut socket, "term3", &third).await;
        open(&mut socket, "in3", "term3").await;
        assert_eq!(input(&mut socket, "in3", 0, text("fresh")).await, ack("in3", 0));
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/pairing-control".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([("device_id".into(), json!("control"))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let revoked = input(&mut socket, "in3", 1, text("revoked")).await;
        assert_eq!(revoked["reason"], "revoked");

        let mut viewer = connect(address, "/v1/client/collections/stream", COLLECTION_SUBPROTOCOL).await;
        send_json(&mut viewer, json!({"kind":"input-open", "id":"ro", "follow":"term"})).await;
        let refused = next_input(&mut viewer).await;
        assert_eq!((refused["id"].as_str(), refused["reason"].as_str()), (Some("ro"), Some("rejected")));

        let screen = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let screen = runtime.screen("input-test").unwrap();
                if screen.contains("accepted:fresh") || std::time::Instant::now() > deadline {
                    return screen;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .await
        .unwrap();
        for line in ["one", "two", "kept", "fresh"] {
            assert!(screen.contains(&format!("accepted:{line}")), "{line} missing: {screen}");
        }
        for line in ["repeat", "gapped", "late", "detached", "replaced", "revoked"] {
            assert!(!screen.contains(&format!("accepted:{line}")), "{line} written: {screen}");
        }
        server.abort();
        let _ = server.await;
    }
}
