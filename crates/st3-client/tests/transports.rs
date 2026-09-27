use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use st3::api::AppState;
use st3::model::{AttentionRequest, ClaimInput};
use st3::store::Store;
use st3_client::{
    AttentionResolveParameters, Capabilities, Client, ClientError, Envelope, ErrorCode, Fence,
    LaunchVariantParameters, PairingBegin, PairingComplete, Resource, TargetParameters,
    TerminalAttachment, TerminalInputMode, TerminalInputParameters, TerminalResizeParameters,
    TimelineBody, TimelineUsageSemantics,
};
use tokio::sync::{Notify, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

fn state(root: &Path, name: &str) -> AppState {
    let pty_binary = root.join(format!("{name}-fake-pty"));
    std::fs::write(
        &pty_binary,
        "#!/bin/sh\nif [ \"$1\" = peek ]; then printf 'terminal ready\\n$ '; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&pty_binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    AppState {
        store: Arc::new(Store::open_memory(name).unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: name.into(),
        state_dir: root.to_path_buf(),
        pty_root: root.join("pty"),
        pty_binary,
        fleet_id: None,
        configured_peers: Vec::new(),
        client_relay: None,
        native_session_home: None,
        planner_default: st3::model::PlannerSpec::default(),
    }
}

fn publish_terminal(state: &AppState, incarnation: &str) {
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/terminal-demo".into(),
            kind: "runtime.observed".into(),
            actor: Some("agent/terminal-demo".into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    Value::String("terminal-demo-runtime".into()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String("running".into())),
                ("terminal".into(), Value::Bool(true)),
                ("reachability".into(), Value::String("local".into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: "agent/terminal-demo".into(),
            kind: "harness.timeline".into(),
            actor: Some("agent/terminal-demo".into()),
            fields: BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                (
                    "entry_id".into(),
                    Value::String(format!("timeline-entry/usage-{incarnation}")),
                ),
                ("sequence".into(), Value::from(1)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("system".into())),
                ("entry_type".into(), Value::String("usage".into())),
                ("final".into(), Value::Bool(true)),
                ("body".into(), serde_json::json!({"total_tokens":7})),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("timeline-usage-{incarnation}")),
        })
        .unwrap();
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("Unix server socket {} did not appear", socket.display());
}

fn assert_private_socket(socket: &Path) {
    let mode = std::fs::metadata(socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode,
        0o600,
        "Unix socket {} must be accessible only to its owner",
        socket.display()
    );
}

async fn unix_status(socket: &Path, path: &str, credential: Option<&str>) -> StatusCode {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    });
    let mut request = Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "localhost");
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    sender
        .send_request(request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn attach_terminal(client: &Client, suffix: &str) -> TerminalAttachment {
    let runtimes = client.runtimes_list(None, None, false).await.unwrap();
    let runtime = runtimes
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Runtime(runtime) if runtime.terminal_id.is_some() => Some(runtime),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "terminal-capable runtime advertised by the server: {:?}",
                runtimes.value.items
            )
        });
    let terminal_id = runtime.terminal_id.as_deref().unwrap();
    let incarnation = runtime.incarnation_id.as_deref().unwrap();
    let terminal_sequence = runtime.terminal_sequence.unwrap();
    assert_eq!(runtime.terminal_access.as_ref().unwrap().read, "granted");
    client
        .terminal_attach(
            format!("action/terminal-attach-{suffix}"),
            format!("terminal-attach-{suffix}-00000001"),
            Fence {
                snapshot_id: runtimes.snapshot.id,
                runtime_incarnation: Some(incarnation.into()),
                terminal_sequence: Some(terminal_sequence),
                ..Fence::default()
            },
            TargetParameters {
                target_id: terminal_id.into(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap()
        .value
        .terminal_attachment
        .expect("terminal.attach result")
}

async fn detach_terminal(client: &Client, attachment: &TerminalAttachment, suffix: &str) {
    let capabilities = client.capabilities().await.unwrap();
    client
        .terminal_detach(
            format!("action/terminal-detach-{suffix}"),
            format!("terminal-detach-{suffix}-0000001"),
            Fence {
                snapshot_id: capabilities.snapshot.id,
                runtime_incarnation: Some(attachment.runtime_incarnation.clone()),
                terminal_sequence: Some(capabilities.snapshot.store_index),
                ..Fence::default()
            },
            TargetParameters {
                target_id: attachment.attachment_id.clone(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn generated_client_conforms_over_the_real_unix_transport() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let state = state(root.path(), "client-unix");
    publish_terminal(&state, "terminal-demo-runtime:i1");
    state
        .store
        .request_attention(
            "attention/nathan-client-proof",
            &AttentionRequest {
                reviewer: "person/nathan".into(),
                title: "Client proof".into(),
                reason: "Resolve through the generated client".into(),
                severity: "error".into(),
                targets: vec!["agent/terminal-demo".into()],
                actor: "daemon/runtime".into(),
                idempotency_key: "attention-nathan-client-proof".into(),
            },
        )
        .unwrap();
    state
        .store
        .request_attention(
            "attention/alex-client-proof",
            &AttentionRequest {
                reviewer: "person/alex".into(),
                title: "Other person's attention".into(),
                reason: "Must not resolve as Nathan".into(),
                severity: "warning".into(),
                targets: Vec::new(),
                actor: "daemon/runtime".into(),
                idempotency_key: "attention-alex-client-proof".into(),
            },
        )
        .unwrap();
    let app = st3::api::router(state.clone());
    let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
    wait_for_socket(&socket).await;

    let client = Client::unix_as(&socket, "person/nathan");
    let read_only = Client::unix(&socket);
    assert_eq!(
        read_only.capabilities().await.unwrap().value.session_actor,
        "client/local/read-only"
    );
    assert!(
        read_only
            .pairing_begin(&PairingBegin {
                api_version: st3_client::API_VERSION.into(),
                device_name: "Unattributed device".into(),
                person_id: "person/nathan".into(),
                full_control: None,
            })
            .await
            .is_err(),
        "plain Unix clients cannot infer pairing authority"
    );
    let capabilities = client.capabilities().await.unwrap();
    assert_eq!(
        capabilities.value.transport,
        st3_client::TransportKind::Unix
    );
    assert_eq!(capabilities.value.session_actor, "person/nathan");
    let work = client.work_list(None, Some(7), false).await.unwrap();
    assert_eq!(work.value.collection, "work");
    assert_eq!(work.value.page.limit, 7);
    let events = client.events(None, Some(10), Some(0)).await.unwrap();
    assert!(!events.value.has_more);

    let attention_page = client.attention_list(None, None, false).await.unwrap();
    let attention = attention_page
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Attention(attention)
                if attention.header.id == "attention/nathan-client-proof" =>
            {
                Some(attention)
            }
            _ => None,
        })
        .expect("person/nathan attention in generated client page");
    let resolve_fence = Fence {
        snapshot_id: attention_page.snapshot.id.clone(),
        subject_revisions: BTreeMap::from([(
            attention.header.id.clone(),
            attention.header.revision.clone(),
        )]),
        ..Fence::default()
    };
    let resolve_parameters = AttentionResolveParameters {
        attention_id: attention.header.id.clone(),
        outcome: "resolved".into(),
        reason: Some("generated client proof".into()),
    };
    assert!(
        read_only
            .attention_resolve(
                "action/attention-resolve-read-only",
                "attention-resolve-read-only-0001",
                resolve_fence.clone(),
                resolve_parameters.clone(),
            )
            .await
            .is_err(),
        "plain Unix clients cannot resolve person attention"
    );
    assert!(
        read_only
            .launch_approve(
                "action/launch-approve-without-person",
                "launch-approve-no-person-0001",
                Fence {
                    snapshot_id: attention_page.snapshot.id.clone(),
                    ..Fence::default()
                },
                LaunchVariantParameters {
                    launch_id: "launch/example".into(),
                    variant_id: "launch-variant/example/default".into(),
                },
            )
            .await
            .is_err(),
        "plain Unix clients cannot approve launches"
    );
    let sessions = read_only.sessions_list(None, None, false).await.unwrap();
    let session = sessions
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Session(session) if session.owner_id == "agent/terminal-demo" => {
                Some(session)
            }
            _ => None,
        })
        .expect("the advertised runtime has a typed session resource");
    let timeline = read_only
        .timeline(&session.header.id, None, None)
        .await
        .expect("the generated route strips and encodes the projected session ID");
    let usage = timeline
        .value
        .items
        .iter()
        .find_map(|entry| match &entry.body {
            TimelineBody::Usage(usage) => Some(usage),
            _ => None,
        })
        .expect("source usage without semantics still decodes as a typed usage body");
    assert_eq!(usage.semantics, TimelineUsageSemantics::Response);
    assert_eq!(usage.driver, "codex");
    assert_eq!(usage.total_tokens, Some(7));
    client
        .attention_resolve(
            "action/attention-resolve-generated-client",
            "attention-resolve-client-0001",
            resolve_fence,
            resolve_parameters,
        )
        .await
        .unwrap();
    assert!(
        state
            .store
            .attention_items(Some("person/nathan"))
            .unwrap()
            .is_empty()
    );
    // A person-scoped client cannot enumerate another person's private inbox. Use
    // the read-only local projection to obtain the exact cross-person fence, then
    // prove the named-person mutation is still rejected.
    let alex_page = read_only.attention_list(None, None, false).await.unwrap();
    let alex = alex_page
        .value
        .items
        .iter()
        .find_map(|resource| match resource {
            Resource::Attention(attention)
                if attention.header.id == "attention/alex-client-proof" =>
            {
                Some(attention)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        client
            .attention_resolve(
                "action/attention-resolve-cross-person",
                "attention-resolve-cross-0001",
                Fence {
                    snapshot_id: alex_page.snapshot.id,
                    subject_revisions: BTreeMap::from([(
                        alex.header.id.clone(),
                        alex.header.revision.clone(),
                    )]),
                    ..Fence::default()
                },
                AttentionResolveParameters {
                    attention_id: alex.header.id.clone(),
                    outcome: "resolved".into(),
                    reason: None,
                },
            )
            .await
            .is_err(),
        "person/nathan cannot resolve person/alex attention"
    );

    let read_only_attachment = attach_terminal(&read_only, "unix-read-only").await;
    let read_only_stream = read_only
        .terminal_frames(
            &read_only_attachment.terminal_id,
            None,
            Some(&read_only_attachment.runtime_incarnation),
            read_only_attachment.stream_capability.as_deref().unwrap(),
            Some(1_000),
        )
        .await
        .expect("a plain Unix client may consume its read-only viewer capability");
    assert_eq!(
        read_only_stream.screen.value.lines[0].text,
        "terminal ready"
    );
    let read_only_fence = read_only.capabilities().await.unwrap();
    for denied in [
        read_only
            .terminal_input(
                "action/read-only-terminal-input",
                "read-only-terminal-input-0001",
                Fence {
                    snapshot_id: read_only_fence.snapshot.id.clone(),
                    runtime_incarnation: Some(read_only_attachment.runtime_incarnation.clone()),
                    terminal_sequence: Some(read_only_fence.snapshot.store_index),
                    ..Fence::default()
                },
                TerminalInputParameters {
                    terminal_id: read_only_attachment.terminal_id.clone(),
                    mode: TerminalInputMode::Line,
                    value: "must not be written".into(),
                },
            )
            .await,
        read_only
            .terminal_resize(
                "action/read-only-terminal-resize",
                "read-only-terminal-resize-0001",
                Fence {
                    snapshot_id: read_only_fence.snapshot.id.clone(),
                    runtime_incarnation: Some(read_only_attachment.runtime_incarnation.clone()),
                    terminal_sequence: Some(read_only_fence.snapshot.store_index),
                    ..Fence::default()
                },
                TerminalResizeParameters {
                    terminal_id: read_only_attachment.terminal_id.clone(),
                    rows: 40,
                    columns: 120,
                },
            )
            .await,
    ] {
        assert!(
            matches!(denied, Err(ClientError::Api(ErrorCode::Forbidden, _, _))),
            "terminal input and resize require terminal.control"
        );
    }
    detach_terminal(&read_only, &read_only_attachment, "unix-read-only").await;

    let first_attachment = attach_terminal(&client, "unix-first").await;
    let first = client
        .terminal_frames(
            &first_attachment.terminal_id,
            None,
            Some("terminal-demo-runtime:i1"),
            first_attachment.stream_capability.as_deref().unwrap(),
            Some(1_000),
        )
        .await
        .unwrap();
    assert_eq!(
        first.screen.value.runtime_incarnation,
        "terminal-demo-runtime:i1"
    );
    assert_eq!(first.screen.value.lines[0].text, "terminal ready");
    let frames = first.frames.expect("bounded initial frame page");
    assert_eq!(frames.value.frames.len(), 1);
    assert_eq!(
        frames.value.frames[0].frame_type,
        st3_client::TerminalFrameType::Resync
    );
    assert_eq!(
        frames.value.frames[0].runtime_incarnation,
        "terminal-demo-runtime:i1"
    );

    let control_fence = client.capabilities().await.unwrap();
    let input = client
        .terminal_input(
            "action/person-terminal-input",
            "person-terminal-input-0001",
            Fence {
                snapshot_id: control_fence.snapshot.id.clone(),
                runtime_incarnation: Some(first_attachment.runtime_incarnation.clone()),
                terminal_sequence: Some(control_fence.snapshot.store_index),
                ..Fence::default()
            },
            TerminalInputParameters {
                terminal_id: first_attachment.terminal_id.clone(),
                mode: TerminalInputMode::Line,
                value: "actor attribution proof".into(),
            },
        )
        .await;
    assert!(
        input.is_err(),
        "the fake PTY cannot accept input, but the durable request/result path must run"
    );
    let control_claims = state
        .store
        .claims_for("agent/terminal-demo", None)
        .unwrap()
        .into_iter()
        .filter(|claim| {
            matches!(
                claim.kind.as_str(),
                "terminal.input.requested" | "terminal.input.result"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(control_claims.len(), 2);
    assert!(
        control_claims
            .iter()
            .all(|claim| claim.actor.as_deref() == Some("person/nathan")),
        "terminal control attribution must come from the authenticated session: {control_claims:?}"
    );

    assert!(
        client
            .terminal_frames(
                &first_attachment.terminal_id,
                None,
                Some("terminal-demo-runtime:wrong"),
                attach_terminal(&client, "unix-wrong")
                    .await
                    .stream_capability
                    .as_deref()
                    .unwrap(),
                Some(100),
            )
            .await
            .is_err(),
        "a stale incarnation must be rejected during the WebSocket handshake"
    );

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let mut wrong_protocol = "ws://localhost/v1/client/terminals/agent/terminal-demo/stream"
        .into_client_request()
        .unwrap();
    wrong_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        "wrong.terminal.protocol".parse().unwrap(),
    );
    assert!(
        tokio_tungstenite::client_async(wrong_protocol, stream)
            .await
            .is_err(),
        "the Unix WebSocket must reject the wrong subprotocol"
    );

    let replacement_fence = attach_terminal(&client, "unix-replaced").await;
    publish_terminal(&state, "terminal-demo-runtime:i2");
    assert!(
        client
            .terminal_frames(
                &replacement_fence.terminal_id,
                None,
                Some("terminal-demo-runtime:i1"),
                replacement_fence.stream_capability.as_deref().unwrap(),
                Some(100),
            )
            .await
            .is_err(),
        "reconnect to a replaced incarnation must reject the old fence"
    );
    let second_attachment = attach_terminal(&client, "unix-second").await;
    let replacement = client
        .terminal_frames(
            &second_attachment.terminal_id,
            None,
            Some("terminal-demo-runtime:i2"),
            second_attachment.stream_capability.as_deref().unwrap(),
            Some(1_000),
        )
        .await
        .unwrap();
    assert_eq!(
        replacement.screen.value.runtime_incarnation,
        "terminal-demo-runtime:i2"
    );
    assert!(
        client
            .terminal_frames(
                &second_attachment.terminal_id,
                None,
                Some("terminal-demo-runtime:i2"),
                second_attachment.stream_capability.as_deref().unwrap(),
                Some(100),
            )
            .await
            .is_err(),
        "a stream capability must be single use"
    );
    let detached = attach_terminal(&client, "unix-detach").await;
    detach_terminal(&client, &detached, "unix-first").await;
    detach_terminal(&client, &detached, "unix-repeat").await;
    assert!(
        client
            .terminal_frames(
                &detached.terminal_id,
                None,
                Some("terminal-demo-runtime:i2"),
                detached.stream_capability.as_deref().unwrap(),
                Some(100),
            )
            .await
            .is_err(),
        "an idempotently detached viewer must not reconnect"
    );
    server.abort();
}

#[tokio::test]
async fn generated_client_conforms_over_paired_loopback_and_rejects_bad_credentials() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path(), "client-loopback");
    publish_terminal(&state, "terminal-demo-runtime:fabric-i1");

    let socket = root.path().join("st3.sock");
    let server_socket = socket.clone();
    let unix_app = st3::api::router(state.clone());
    let unix_server =
        tokio::spawn(async move { st3::api::serve_unix(&server_socket, unix_app).await });
    wait_for_socket(&socket).await;
    let local = Client::unix_as(&socket, "person/nathan");
    let challenge = local
        .pairing_begin(&PairingBegin {
            api_version: st3_client::API_VERSION.into(),
            device_name: "Conformance phone".into(),
            person_id: "person/nathan".into(),
            full_control: None,
        })
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = st3::api::fabric_router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{address}");
    let paired = Client::fabric_pairing(&base)
        .pairing_complete(
            &challenge.value.pairing_id,
            &PairingComplete {
                api_version: st3_client::API_VERSION.into(),
                code: challenge.value.code,
                device_public_key: "conformance-public-key-000000000000000000000000".into(),
            },
        )
        .await
        .unwrap();

    let gateway_socket = root.path().join("st3-client.sock");
    let server_gateway_socket = gateway_socket.clone();
    let gateway_app = st3::api::fabric_router(state.clone());
    let gateway_server =
        tokio::spawn(
            async move { st3::api::serve_unix(&server_gateway_socket, gateway_app).await },
        );
    wait_for_socket(&gateway_socket).await;
    assert_private_socket(&gateway_socket);
    assert_eq!(
        unix_status(&gateway_socket, "/v1/health", None).await,
        StatusCode::OK
    );
    assert_eq!(
        unix_status(&gateway_socket, "/v1/client/capabilities", None).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        Client::unix(&gateway_socket).capabilities().await.is_err(),
        "the paired-only Unix gateway must reject an ordinary local session"
    );
    let direct_gateway = Client::unix_gateway(&gateway_socket, &paired.value.credential);
    let direct_capabilities = direct_gateway.capabilities().await.unwrap();
    assert_eq!(
        direct_capabilities.value.transport,
        st3_client::TransportKind::FabricLoopback
    );
    assert_eq!(
        unix_status(
            &gateway_socket,
            "/v1/schema",
            Some(&paired.value.credential)
        )
        .await,
        StatusCode::FORBIDDEN,
        "the client gateway must never expose privileged non-client routes"
    );
    let direct_attachment = attach_terminal(&direct_gateway, "gateway-unix").await;
    direct_gateway
        .terminal_frames(
            &direct_attachment.terminal_id,
            None,
            Some("terminal-demo-runtime:fabric-i1"),
            direct_attachment.stream_capability.as_deref().unwrap(),
            Some(1_000),
        )
        .await
        .unwrap();

    let http = reqwest::Client::new();

    let client = Client::fabric_loopback(&base, &paired.value.credential);
    let capabilities: Envelope<Capabilities> = client.capabilities().await.unwrap();
    assert_eq!(
        capabilities.value.transport,
        st3_client::TransportKind::FabricLoopback
    );
    assert_eq!(capabilities.value.session_actor, paired.value.session_actor);
    assert_eq!(paired.value.person_id, "person/nathan");
    assert_eq!(
        client
            .operations_list(None, None, false)
            .await
            .unwrap()
            .value
            .collection,
        "operations"
    );

    let denied = http
        .get(format!("{base}/v1/client/capabilities"))
        .bearer_auth("not-a-real-client-credential")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let error: st3_client::ErrorEnvelope = denied.json().await.unwrap();
    assert_eq!(error.code, st3_client::ErrorCode::Forbidden);

    let fabric_attachment = attach_terminal(&client, "fabric-first").await;
    let terminal = client
        .terminal_frames(
            &fabric_attachment.terminal_id,
            None,
            Some("terminal-demo-runtime:fabric-i1"),
            fabric_attachment.stream_capability.as_deref().unwrap(),
            Some(1_000),
        )
        .await
        .unwrap();
    assert_eq!(
        terminal.screen.value.runtime_incarnation,
        "terminal-demo-runtime:fabric-i1"
    );
    let bad_attachment = "not-a-real-stream-capability-0000000000";
    assert!(
        Client::fabric_loopback(&base, "not-a-real-client-credential")
            .terminal_frames(
                &fabric_attachment.terminal_id,
                None,
                Some("terminal-demo-runtime:fabric-i1"),
                bad_attachment,
                Some(100),
            )
            .await
            .is_err(),
        "the Fabric WebSocket must authenticate before upgrade"
    );

    let mut wrong_protocol = format!(
        "ws://{address}/v1/client/terminals/agent%2Fterminal-demo/stream?incarnation=terminal-demo-runtime%3Afabric-i1"
    )
    .into_client_request()
    .unwrap();
    wrong_protocol.headers_mut().insert(
        "sec-websocket-protocol",
        "wrong.terminal.protocol".parse().unwrap(),
    );
    wrong_protocol.headers_mut().insert(
        "authorization",
        format!("Bearer {}", paired.value.credential)
            .parse()
            .unwrap(),
    );
    assert!(
        tokio_tungstenite::connect_async(wrong_protocol)
            .await
            .is_err(),
        "the authenticated Fabric WebSocket must reject the wrong subprotocol"
    );

    let revoked_attachment = attach_terminal(&client, "fabric-revoked").await;
    let local_capabilities = local.capabilities().await.unwrap();
    local
        .pairing_revoke(
            "action/pairing-revoke-fabric",
            "pairing-revoke-fabric-000001",
            Fence {
                snapshot_id: local_capabilities.snapshot.id,
                ..Fence::default()
            },
            TargetParameters {
                target_id: paired.value.device_id.clone(),
                ..TargetParameters::default()
            },
        )
        .await
        .unwrap();
    assert!(
        client
            .terminal_frames(
                &revoked_attachment.terminal_id,
                None,
                Some("terminal-demo-runtime:fabric-i1"),
                revoked_attachment.stream_capability.as_deref().unwrap(),
                Some(100),
            )
            .await
            .is_err(),
        "credential revocation must invalidate an unused stream capability"
    );
    assert!(
        direct_gateway.capabilities().await.is_err(),
        "the revoked bearer must also fail on the paired-only Unix gateway"
    );
    local
        .capabilities()
        .await
        .expect("the privileged local socket remains independent");

    server.abort();
    gateway_server.abort();
    unix_server.abort();
    let _ = server.await;
    let _ = gateway_server.await;
    let _ = unix_server.await;

    let restarted_local_socket = socket.clone();
    let restarted_local_app = st3::api::router(state.clone());
    let restarted_local = tokio::spawn(async move {
        st3::api::serve_unix(&restarted_local_socket, restarted_local_app).await
    });
    let restarted_gateway_socket = gateway_socket.clone();
    let restarted_gateway_app = st3::api::fabric_router(state);
    let restarted_gateway = tokio::spawn(async move {
        st3::api::serve_unix(&restarted_gateway_socket, restarted_gateway_app).await
    });
    wait_for_socket(&socket).await;
    wait_for_socket(&gateway_socket).await;
    assert_private_socket(&socket);
    assert_private_socket(&gateway_socket);
    Client::unix_as(&socket, "person/nathan")
        .capabilities()
        .await
        .expect("the privileged local socket restarts independently");
    assert_eq!(
        unix_status(&gateway_socket, "/v1/health", None).await,
        StatusCode::OK
    );
    assert_eq!(
        unix_status(&gateway_socket, "/v1/client/capabilities", None).await,
        StatusCode::FORBIDDEN
    );
    restarted_gateway.abort();
    restarted_local.abort();
}
