use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use st3::{api::AppState, model::ClaimInput, store::Store};
use st3_client::{
    Client, CollectionEvent, GlassBody, GlassDelete, GlassLayout, GlassPut, GlassTab,
};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

fn state(root: &Path) -> AppState {
    AppState {
        store: Arc::new(Store::open_memory("glasses-test").unwrap()),
        notify: Arc::new(Notify::new()),
        event_notify: watch::channel(0).0,
        node: "glasses-test".into(),
        state_dir: root.into(),
        pty_root: root.join("pty"),
        pty_binary: "pty".into(),
        fleet_id: None,
        configured_peers: vec![],
        client_relay: None,
        native_session_home: None,
        planner_default: Default::default(),
        private_notes: Default::default(),
    }
}
async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
    person: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(person) = person {
        request = request.header("x-st3-person", person);
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap(),
    )
}
#[tokio::test]
async fn legacy_glass_reads_convert_and_client_writes_require_the_current_shape() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let id = "019a0000-0000-7000-8000-000000000004";
    let old = json!({"name":"Home only","tabs":[]});
    let original = state
        .store
        .append_claim(&ClaimInput {
            subject: format!("glass/person/ada/{id}"),
            kind: "glass.upserted".into(),
            actor: Some("person/ada".into()),
            fields: serde_json::from_value(json!({"body":old,"base_revision":null})).unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    let app = st3::api::router(state.clone());
    let path = format!("/v1/client/glasses/{id}");
    let (status, read) = request(
        app.clone(),
        "GET",
        &path,
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["value"]["revision"], original.id);
    assert_eq!(
        read["value"]["body"],
        json!({"name":"Home only","layout":{"tabs":[]}})
    );
    let (status, list) = request(
        app.clone(),
        "GET",
        "/v1/client/glasses",
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["value"]["items"][0], read["value"]);
    let (status, _) = request(
        app.clone(),
        "PUT",
        &path,
        Some("person/ada"),
        Some("old-shape"),
        json!({"body":old,"base_revision":original.id}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, saved) = request(
        app,
        "PUT",
        &path,
        Some("person/ada"),
        Some("converted-shape"),
        json!({"body":read["value"]["body"],"base_revision":original.id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["value"]["replaced_revision"], original.id);
    let claims = state
        .store
        .claims_for(&original.subject, Some("glass.upserted"))
        .unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].body["fields"]["body"], old);
    assert_eq!(claims[1].body["fields"]["body"], read["value"]["body"]);
}

#[tokio::test]
async fn glasses_routes_enforce_owner_and_mutation_contract_and_hide_raw_history() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let app = st3::api::router(state.clone());
    let id = "019a0000-0000-7000-8000-000000000001";
    let path = format!("/v1/client/glasses/{id}");
    let body = json!({"name":"Main workspace", "layout":{"split":"right","ratio":0.3,"children":[{"tabs":[{"title":"Work","pane":"agent:opaque"},{"pane":"mission:opaque"}]},{"tabs":[]}]}});
    for (index, ratio) in [json!(0.099), json!(0.901), json!(null), json!("0.3")]
        .into_iter()
        .enumerate()
    {
        let mut invalid = body.clone();
        invalid["layout"]["ratio"] = ratio;
        assert_eq!(
            request(
                app.clone(),
                "PUT",
                &path,
                Some("person/ada"),
                Some(&format!("invalid-ratio-{index}")),
                json!({"body":invalid,"base_revision":null})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for person in [None, Some("agent/worker")] {
        assert_eq!(
            request(
                app.clone(),
                "GET",
                "/v1/client/glasses",
                person,
                None,
                Value::Null
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(
                app.clone(),
                "PUT",
                &path,
                person,
                Some("denied"),
                json!({"body":body,"base_revision":null})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        request(
            app.clone(),
            "PUT",
            &path,
            Some("person/ada"),
            None,
            json!({"body":body,"base_revision":null})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, created) = request(
        app.clone(),
        "PUT",
        &path,
        Some("person/ada"),
        Some("create"),
        json!({"body":body,"base_revision":null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let revision = created["value"]["revision"].as_str().unwrap();
    let (_, retry) = request(
        app.clone(),
        "PUT",
        &path,
        Some("person/ada"),
        Some("create"),
        json!({"body":body,"base_revision":null}),
    )
    .await;
    assert_eq!(retry["value"]["revision"], revision);
    let (conflict_status,conflict)=request(app.clone(),"PUT",&path,Some("person/ada"),Some("create"),json!({"body":{"name":"Changed retry","layout":{"tabs":[{"pane":"home:"}]}},"base_revision":null})).await;
    assert_eq!(conflict_status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "idempotency-conflict");
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &path,
            Some("person/alex"),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, other) = request(
        app.clone(),
        "GET",
        "/v1/client/glasses",
        Some("person/alex"),
        None,
        Value::Null,
    )
    .await;
    assert!(other["value"]["items"].as_array().unwrap().is_empty());
    let (_, mine) = request(
        app.clone(),
        "GET",
        "/v1/client/glasses",
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(mine["value"]["items"][0]["body"], body);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &format!(
                "/v1/claims/by-id/{}",
                revision.strip_prefix("claim/").unwrap_or(revision)
            ),
            None,
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, raw) = request(app.clone(), "GET", "/v1/claims", None, None, Value::Null).await;
    assert!(
        raw.get("claims").is_some() || raw.pointer("/value/claims").is_some(),
        "{raw}"
    );
    assert!(
        raw.pointer("/value/claims")
            .or_else(|| raw.get("claims"))
            .expect("claim page")
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, history) = request(
        app.clone(),
        "GET",
        "/v1/client/history",
        None,
        None,
        Value::Null,
    )
    .await;
    assert!(history["value"]["items"].as_array().unwrap().is_empty());
    let (_, caps) = request(
        app.clone(),
        "GET",
        "/v1/client/capabilities",
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    assert!(
        caps["value"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "glasses" && c["version"] == 2 && c["state"] == "granted")
    );
    let empty_body = json!({"name":"Home only","layout":{"tabs":[]}});
    let (empty_status, empty) = request(
        app.clone(),
        "PUT",
        &path,
        Some("person/ada"),
        Some("close-last-tab"),
        json!({"body":empty_body,"base_revision":revision}),
    )
    .await;
    assert_eq!(empty_status, StatusCode::OK);
    assert_eq!(empty["value"]["body"], empty_body);
    let (_, empty_read) = request(
        app.clone(),
        "GET",
        &path,
        Some("person/ada"),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(empty_read["value"]["body"], empty_body);
    let (_, deleted) = request(
        app.clone(),
        "DELETE",
        &path,
        Some("person/ada"),
        Some("delete"),
        json!({"base_revision":revision}),
    )
    .await;
    assert_eq!(deleted["value"]["deleted"], true);
    assert_eq!(
        request(
            app.clone(),
            "GET",
            &path,
            Some("person/ada"),
            None,
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_ne!(
        request(
            app.clone(),
            "PUT",
            &path,
            Some("person/ada"),
            Some("reuse"),
            json!({"body":body,"base_revision":null})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn glasses_rust_client_and_collection_stream_deliver_upserts_and_removes() {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("client.sock");
    let state = state(root.path());
    let legacy = state.store.append_claim(&ClaimInput {
        subject: "glass/person/ada/019a0000-0000-7000-8000-000000000003".into(),
        kind: "glass.upserted".into(),
        actor: Some("person/ada".into()),
        fields: serde_json::from_value(json!({"body":{"name":"Legacy","tabs":[
            {"title":"Work","layout":{"split":"right","children":[{"pane":"old:a"},{"pane":"old:b"}]}}
        ]},"base_revision":null})).unwrap(),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).unwrap();
    let app = st3::api::router(state.clone());
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
    let anonymous = Client::unix(&socket);
    let mut denied_stream = anonymous.collection_stream().await.unwrap();
    denied_stream.subscribe_glasses("denied").await.unwrap();
    assert!(matches!(
        denied_stream.next_event().await.unwrap().unwrap(),
        CollectionEvent::Error {
            code: Some(st3_client::ErrorCode::Forbidden),
            ..
        }
    ));
    denied_stream.close().await;
    let client = Client::unix_as(&socket, "person/ada");
    let mut stream = client.collection_stream().await.unwrap();
    stream.subscribe_glasses("glasses").await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let CollectionEvent::Snapshot { items, .. } = first else {
        panic!("expected initial glasses snapshot")
    };
    assert_eq!(items.len(), 1);
    let st3_client::Resource::Glass(glass) = &items[0] else {
        panic!("expected a typed glass")
    };
    assert_eq!(glass.header.revision, legacy.id);
    assert_eq!(
        glass.body.as_ref().unwrap().layout,
        GlassLayout::Group {
            tabs: vec![
                GlassTab {
                    title: Some("Work".into()),
                    pane: "old:a".into()
                },
                GlassTab {
                    title: None,
                    pane: "old:b".into()
                },
            ]
        }
    );
    let id = "019a0000-0000-7000-8000-000000000002";
    let body = GlassBody {
        name: "Main".into(),
        layout: GlassLayout::Group {
            tabs: vec![GlassTab {
                title: None,
                pane: "opaque:first".into(),
            }],
        },
    };
    let created = client
        .put_glass(
            id,
            &GlassPut {
                body: body.clone(),
                base_revision: None,
            },
            "create",
        )
        .await
        .unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(change,CollectionEvent::Changes{upserts,..} if upserts.len()==1));
    assert_eq!(client.get_glass(id).await.unwrap().value.body, Some(body));
    assert_eq!(
        client
            .list_glasses(None, Some(100))
            .await
            .unwrap()
            .value
            .items
            .len(),
        2
    );
    client
        .delete_glass(
            id,
            &GlassDelete {
                base_revision: Some(created.value.header.revision),
            },
            "delete",
        )
        .await
        .unwrap();
    let change = tokio::time::timeout(Duration::from_secs(5), stream.next_event())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(change,CollectionEvent::Changes{removes,..} if removes.len()==1));
    stream.close().await;
    let challenge = client
        .pairing_begin(&st3_client::PairingBegin {
            api_version: "st3.client.v0".into(),
            device_name: "Fixture phone".into(),
            person_id: "person/ada".into(),
            full_control: None,
            scopes: None,
        })
        .await
        .unwrap()
        .value;
    let paired = client
        .pairing_complete(
            &challenge.pairing_id,
            &st3_client::PairingComplete {
                api_version: "st3.client.v0".into(),
                code: challenge.code,
                device_public_key: "a".repeat(64),
                key_storage: None,
            },
        )
        .await
        .unwrap()
        .value;
    let gateway_socket = root.path().join("gateway.sock");
    let gateway_server_socket = gateway_socket.clone();
    let gateway = tokio::spawn(async move {
        st3::api::serve_unix(&gateway_server_socket, st3::api::fabric_router(state))
            .await
            .unwrap();
    });
    for _ in 0..100 {
        if gateway_socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let phone = Client::unix_gateway(&gateway_socket, paired.credential);
    let id = "019a0000-0000-7000-8000-000000000003";
    let body = GlassBody {
        name: "Phone workspace".into(),
        layout: GlassLayout::Group { tabs: vec![] },
    };
    phone
        .put_glass(
            id,
            &GlassPut {
                body: body.clone(),
                base_revision: None,
            },
            "phone-create",
        )
        .await
        .unwrap();
    assert_eq!(client.get_glass(id).await.unwrap().value.body, Some(body));
    assert_eq!(
        phone
            .list_glasses(None, None)
            .await
            .unwrap()
            .value
            .items
            .len(),
        1
    );
    let mut phone_stream = phone.collection_stream().await.unwrap();
    phone_stream.subscribe_glasses("phone").await.unwrap();
    assert!(
        matches!(phone_stream.next_event().await.unwrap().unwrap(),CollectionEvent::Snapshot{items,..} if items.len()==1)
    );
    phone_stream.close().await;
    gateway.abort();
    server.abort();
}
