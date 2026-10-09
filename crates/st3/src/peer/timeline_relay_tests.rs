//! Exercise the actual typed owner-client and signed relay, then the gateway's raw collection
//! frames. The owner stub supplies published-schema cases; it does not simulate serde itself.
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use axum::routing::get;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use smallclaims::sync::{FleetContext, PeerState, peer_router};
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use tower::ServiceExt as _;

fn validator(definition: &str) -> jsonschema::Validator {
    let mut schema: Value = serde_json::from_str(include_str!(
        "../../../../docs/st3/client-v0/schemas/client-v0.schema.json"
    ))
    .unwrap();
    schema.as_object_mut().unwrap().remove("oneOf");
    schema["$ref"] = json!(format!("#/$defs/{definition}"));
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
}

fn conform(validator: &jsonschema::Validator, value: &Value) {
    let errors: Vec<_> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "raw wire schema errors: {errors:?}");
}

#[tokio::test]
async fn relay_omission_preserves_raw_pages_changes_and_collection_frames() {
    let root = tempfile::tempdir().unwrap();
    let agent = "agent/relay-omission";
    let incarnation = "relay-runtime:i1";
    let session_id = format!(
        "session/{}",
        &hex::encode(Sha256::digest(format!("{agent}:{incarnation}").as_bytes()))[..24]
    );
    let mut local: Value = serde_json::from_str(include_str!(
        "../../../../docs/st3/client-v0/fixtures/timeline-relay-omission.json"
    ))
    .unwrap();
    local["value"]["session_id"] = json!(session_id);
    let mut changes = local.clone();
    changes["value"] = json!({"kind":"conversation-changes", "session_id":session_id,
        "items":local["value"]["items"], "next_cursor":"conversation-cursor/relay/next"});
    let page_validator = validator("TimelinePage");
    let changes_validator = validator("ConversationChanges");
    let entry_validator = validator("TimelineEntry");
    let frame_validator = validator("CollectionFrame");
    conform(&page_validator, &local["value"]);
    conform(&changes_validator, &changes["value"]);
    // Negative controls: adding null to any omitted, nonnullable field fails the published
    // schema. This also covers usage fields beyond those populated in the original fixture.
    let definitions = [
        (0, &["from", "to", "title"][..]),
        (2, &["attachment_id"][..]),
        (3, &["text"][..]),
        (5, &["detail"][..]),
        (
            7,
            &[
                "model",
                "input_tokens",
                "output_tokens",
                "cached_tokens",
                "cache_write_tokens",
                "turn_id",
                "context_used_tokens",
                "context_window_tokens",
                "context_used_percent",
                "compactions",
                "last_compaction_ms",
                "last_compaction_trigger",
                "cost",
                "currency",
            ][..],
        ),
        (14, &["total_tokens"][..]),
        (9, &["withheld_items"][..]),
        (11, &["continuation_cursor"][..]),
    ];
    for (index, fields) in definitions {
        for field in fields {
            let mut invalid = local["value"]["items"][index].clone();
            invalid["body"][*field] = Value::Null;
            assert!(
                !entry_validator.is_valid(&invalid),
                "accepted forbidden null: {field}"
            );
        }
    }
    let local_page = local.clone();
    let local_changes = changes.clone();
    let owner_app = Router::new()
        .route(
            "/v1/client/sessions/{*id}",
            get(move |headers: HeaderMap| {
                let local_page = local_page.clone();
                async move {
                    assert_eq!(headers["x-st3-person"], "person/example");
                    axum::Json(local_page)
                }
            }),
        )
        .route(
            "/v1/client/conversations/{id}/changes",
            get(move |headers: HeaderMap| {
                let local_changes = local_changes.clone();
                async move {
                    assert_eq!(headers["x-st3-person"], "person/example");
                    axum::Json(local_changes)
                }
            }),
        );
    let owner_socket = root.path().join("owner.sock");
    let served = owner_socket.clone();
    let owner_server = tokio::spawn(async move {
        crate::api::serve_unix(&served, owner_app).await.unwrap();
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&owner_socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let auth = FleetAuth::test("fleet-test", &[4; 32]);
    let peer = PeerState::new(
        MainBackend::new(owner_socket),
        "owner".into(),
        auth.clone(),
        FleetContext::legacy(BTreeSet::from(["gateway".into()])),
    );
    for (operation, expected, schema) in [
        (
            ClientReadOperation::Timeline {
                session_id: session_id.clone(),
                limit: 200,
                cursor: Some("timeline-cursor/relay/older".into()),
            },
            &local["value"],
            &page_validator,
        ),
        (
            ClientReadOperation::ConversationChanges {
                session_id: session_id.clone(),
                after: Some("conversation-cursor/relay/before".into()),
                wait_ms: 0,
            },
            &changes["value"],
            &changes_validator,
        ),
    ] {
        let body = serde_json::to_vec(&ClientReadRequest {
            authority_actor: "person/example".into(),
            relay: None,
            request: operation,
        })
        .unwrap();
        let mut request = Request::builder()
            .method("POST")
            .uri(CLIENT_READ_PATH)
            .body(Body::from(body.clone()))
            .unwrap();
        *request.headers_mut() = auth
            .request_headers_for(CLIENT_READ_PATH, "gateway", &body)
            .unwrap();
        let response = peer_router(peer.clone(), smalltalk_routes())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_CLIENT_READ_BYTES)
            .await
            .unwrap();
        auth.verify(
            &headers,
            "RESPONSE",
            CLIENT_READ_PATH,
            &bytes,
            Some("owner"),
            Some(&FleetAuth::body_digest(&body)),
        )
        .unwrap();
        let raw: Value = serde_json::from_slice(&bytes).unwrap();
        conform(schema, &raw["value"]);
        assert_eq!(
            raw["value"], *expected,
            "signed relay changed the owner wire value"
        );
    }
    // The actual gateway collection producer consumes the signed relay. Inspect its raw
    // WebSocket JSON before any generated consumer can turn a forbidden null back into None.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer_server = tokio::spawn(async move {
        axum::serve(listener, peer_router(peer, smalltalk_routes()))
            .await
            .unwrap();
    });
    let owner_store = Store::open_memory("owner").unwrap();
    owner_store
        .append_claim(&crate::model::ClaimInput {
            subject: agent.into(),
            kind: "runtime.observed".into(),
            actor: Some(agent.into()),
            fields: serde_json::from_value(
                json!({"runtime_id":"relay-runtime", "incarnation_id":incarnation,
            "status":"running", "terminal":false}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let gateway_store = Arc::new(Store::open_memory("gateway").unwrap());
    gateway_store
        .import_replication("owner", &owner_store.export_replication(0).unwrap())
        .unwrap();
    let secret = root.path().join("fleet-secret");
    fs::write(&secret, [4_u8; 32]).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    let state = crate::api::AppState {
        store: gateway_store,
        notify: Arc::new(tokio::sync::Notify::new()),
        event_notify: watch::channel(0_u64).0,
        node: "gateway".into(),
        state_dir: root.path().into(),
        pty_root: root.path().join("pty"),
        pty_binary: "unused-pty".into(),
        fleet_id: None,
        configured_peers: vec!["owner".into()],
        native_session_home: None,
        client_relay: ClientRelay::from_config(&Config {
            node: "gateway".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![PeerConfig {
                name: "owner".into(),
                url: format!("http://{address}"),
            }],
            ..Default::default()
        })
        .unwrap(),
        planner_default: crate::model::PlannerSpec::default(),
    };
    let gateway_socket = root.path().join("gateway.sock");
    let served = gateway_socket.clone();
    let gateway_server = tokio::spawn(async move {
        crate::api::serve_unix(&served, crate::api::router(state))
            .await
            .unwrap();
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&gateway_socket)
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = "ws://localhost/v1/client/collections/stream"
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-st3-person", "person/example".parse().unwrap());
    request.headers_mut().insert(
        "sec-websocket-protocol",
        "st3.client.collections.v0".parse().unwrap(),
    );
    let stream = tokio::net::UnixStream::connect(&gateway_socket)
        .await
        .unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"kind":"subscribe", "id":"chat",
        "collection":"conversation", "conversation":agent})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    for replace in [true, false] {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let raw: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        conform(&frame_validator, &raw);
        assert_eq!(raw["kind"], "conversation", "{raw}");
        assert_eq!(raw["id"], "chat");
        assert_eq!(raw["session_id"], session_id);
        assert_eq!(raw["replace"], replace);
        assert_eq!(raw["items"], local["value"]["items"]);
        for entry in raw["items"].as_array().unwrap() {
            conform(&entry_validator, entry);
        }
        if replace {
            assert_eq!(raw["has_more"], true);
        }
    }
    socket.close(None).await.unwrap();
    gateway_server.abort();
    peer_server.abort();
    owner_server.abort();
}
