use super::*;

fn seed(state: &AppState, count: usize, person: &str) {
    let intent = crate::graph::parse_internal_intent(
        "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
        state.store.origin(),
    )
    .unwrap();
    state
        .store
        .apply_internal(&intent, "history-asker")
        .unwrap();
    for n in 0..count {
        let ask = state
            .store
            .ask_person(&PersonAskRequest {
                legacy_request: None,
                person: person.into(),
                title: format!("Question {n}"),
                reason: "Pick a date".into(),
                actor: "agent/node.asker".into(),
                step: None,
                new_run: Some(format!("history-{person}-{n}")),
                incarnation: None,
                idempotency_key: format!("history-{person}-{n}"),
                request: None,
            })
            .unwrap();
        state
            .store
            .finish_person_step(
                &PersonStepResponse {
                    delegation: None,
                    subject: ask.subject,
                    actor: person.into(),
                    summary: format!("Answer {n}"),
                    evidence: vec![],
                    episode: None,
                    idempotency_key: format!("history-answer-{person}-{n}"),
                    answer: None,
                },
                false,
            )
            .unwrap();
    }
    state.store.drain_attention_history();
}

fn pair(state: &AppState, credential: &str, person: &str, scopes: Value) {
    state
        .store
        .append_claim(&ClaimInput {
            subject: format!("custom/client/{credential}"),
            kind: "custom.client.pairing-completed".into(),
            actor: Some(person.into()),
            fields: BTreeMap::from([
                (
                    "credential_hash".into(),
                    json!(hex::encode(Sha256::digest(credential.as_bytes()))),
                ),
                (
                    "session_actor".into(),
                    json!(format!("client/{credential}")),
                ),
                ("person_id".into(), json!(person)),
                ("scopes".into(), scopes),
                (
                    "expires_at_unix_ms".into(),
                    json!(client_now_ms() as u64 + 600_000),
                ),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

async fn read(app: &Router, path: &str, credential: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("authorization", format!("Bearer {credential}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn history_http_is_open_compatible_person_scoped_and_snapshot_pinned() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    seed(&state, 7, "person/avery");
    seed(&state, 2, "person/other");
    pair(
        &state,
        "reader",
        "person/avery",
        json!(["read.projections"]),
    );
    pair(
        &state,
        "other-reader",
        "person/other",
        json!(["read.projections"]),
    );
    pair(
        &state,
        "another-device",
        "person/avery",
        json!(["read.projections"]),
    );
    let app = fabric_router(state.clone());
    let (status, open) = read(&app, "/v1/client/attention", "reader").await;
    assert_eq!(status, StatusCode::OK, "{open}");
    assert!(open["value"]["items"].as_array().unwrap().is_empty());
    assert!(open["value"].get("history").is_none());
    let base = "/v1/client/attention?history=true&state=resolved&limit=2";
    let (status, first) = read(&app, base, "reader").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["value"]["history"]["complete"], false);
    let pinned = first["snapshot"].clone();
    let mut rows = first["value"]["items"].as_array().unwrap().clone();
    let cursor = first["value"]["page"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, on_other_device) = read(&app, base, "another-device").await;
    assert_eq!(on_other_device["value"]["items"], first["value"]["items"]);
    // Later writes and newer source closures do not appear in the pinned continuation.
    seed(&state, 8, "person/avery");
    let mut next = Some(cursor.clone());
    let mut pages = 0;
    while let Some(cursor) = next {
        pages += 1;
        assert!(
            pages < 20,
            "pagination must advance even across invisible versions"
        );
        let path = format!("{base}&cursor={}", urlencoding::encode(&cursor));
        let (status, page) = read(&app, &path, "reader").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["snapshot"], pinned);
        rows.extend(page["value"]["items"].as_array().unwrap().clone());
        next = page["value"]["page"]["next_cursor"]
            .as_str()
            .map(str::to_owned);
    }
    assert_eq!(rows.len(), 7);
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        7
    );
    assert!(rows.iter().all(|r| r["person_id"] == "person/avery"
        && r["state"] == "resolved"
        && r["actions"] == json!([])));
    assert!(
        rows.windows(2)
            .all(|w| w[0]["updated_at"].as_str() >= w[1]["updated_at"].as_str())
    );
    let continuation = format!("{base}&cursor={}", urlencoding::encode(&cursor));
    assert_eq!(
        read(&app, &continuation, "other-reader").await.0,
        StatusCode::GONE
    );
    assert_eq!(
        read(
            &app,
            "/v1/client/attention?history=true&state=resolved&person=person%2Fother",
            "reader"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let detail = format!(
        "/v1/client/attention/{}?history=true&state=resolved",
        urlencoding::encode(rows[0]["id"].as_str().unwrap())
    );
    let (status, item) = read(&app, &detail, "reader").await;
    assert_eq!(status, StatusCode::OK, "{item}");
    assert_eq!(item["value"]["id"], rows[0]["id"]);
    assert_eq!(
        read(&app, &detail, "other-reader").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn history_cursor_authenticates_filters_position_and_projection_scope() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    seed(&state, 3, "person/avery");
    pair(
        &state,
        "reader",
        "person/avery",
        json!(["read.projections"]),
    );
    pair(&state, "no-scope", "person/avery", json!([]));
    let app = fabric_router(state.clone());
    let base = "/v1/client/attention?history=true&state=resolved&limit=1";
    assert_eq!(read(&app, base, "no-scope").await.0, StatusCode::FORBIDDEN);
    let (_, first) = read(&app, base, "reader").await;
    let encoded = first["value"]["page"]["next_cursor"].as_str().unwrap();
    let original = decode_client_cursor(encoded).unwrap();
    for tamper in 0..5 {
        let mut cursor = original.clone();
        match tamper {
            0 => cursor.after_key.as_mut().unwrap().0 = 1,
            1 => cursor.person = Some("person/other".into()),
            2 => cursor.snapshot.store_index = 0,
            3 => cursor.expires_at_unix_ms = u128::MAX,
            _ => cursor.history_snapshot.as_mut().unwrap().version += 1,
        }
        let encoded = encode_client_cursor(&cursor).unwrap();
        let path = format!("{base}&cursor={}", urlencoding::encode(&encoded));
        let (status, body) = read(&app, &path, "reader").await;
        assert_eq!(status, StatusCode::GONE, "{body}");
        assert_eq!(body["code"], "page-cursor-expired");
    }
    for change in [
        "limit=2",
        "state=open",
        "history=false",
        "actor=agent%2Fother",
    ] {
        let mut parts = base
            .split('?')
            .nth(1)
            .unwrap()
            .split('&')
            .filter(|p| p.split('=').next() != change.split('=').next())
            .collect::<Vec<_>>();
        parts.push(change);
        let path = format!(
            "/v1/client/attention?{}&cursor={}",
            parts.join("&"),
            urlencoding::encode(encoded)
        );
        let (status, body) = read(&app, &path, "reader").await;
        assert!(!status.is_success(), "{body}");
    }
    let reused = format!("{base}&cursor={}", urlencoding::encode(encoded));
    let one = read(&app, &reused, "reader").await;
    let two = read(&app, &reused, "reader").await;
    assert_eq!(one.0, StatusCode::OK);
    assert_eq!(one.1["value"]["items"], two.1["value"]["items"]);
}

#[tokio::test]
async fn non_person_history_requires_explicit_person_and_never_reads_globally() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    seed(&state, 2, "person/avery");
    seed(&state, 2, "person/other");
    let app = router(state);
    async fn local_read(app: &Router, path: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
    let base = "/v1/client/attention?history=true&state=resolved";
    let (status, denied) = local_read(&app, base).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    assert_eq!(denied["code"], "forbidden");
    let (status, page) = local_read(&app, &format!("{base}&person=person%2Favery")).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let rows = page["value"]["items"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["person_id"] == "person/avery"));
    let id = urlencoding::encode(rows[0]["id"].as_str().unwrap());
    assert_eq!(
        local_read(&app, &format!("/v1/client/attention/{id}?history=true"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        local_read(
            &app,
            &format!("/v1/client/attention/{id}?history=true&person=person%2Fother")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn history_reset_expires_authenticated_cursor_until_background_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    seed(&state, 3, "person/avery");
    pair(
        &state,
        "reader",
        "person/avery",
        json!(["read.projections"]),
    );
    let app = fabric_router(state.clone());
    let base = "/v1/client/attention?history=true&state=resolved&limit=1";
    let (_, first) = read(&app, base, "reader").await;
    let cursor = first["value"]["page"]["next_cursor"].as_str().unwrap();
    state.store.replay_replication_graph().unwrap();
    let path = format!("{base}&cursor={}", urlencoding::encode(cursor));
    let (status, expired) = read(&app, &path, "reader").await;
    assert_eq!(status, StatusCode::GONE, "{expired}");
    assert_eq!(expired["code"], "page-cursor-expired");
    let (status, indexing) = read(&app, base, "reader").await;
    assert_eq!(status, StatusCode::OK, "{indexing}");
    assert!(indexing["value"]["items"].as_array().unwrap().is_empty());
    assert!(
        indexing["value"]["history"]["note"]
            .as_str()
            .unwrap()
            .contains("still indexing")
    );
    state.store.drain_attention_history();
    let (_, rebuilt) = read(&app, base, "reader").await;
    assert_eq!(rebuilt["value"]["items"], first["value"]["items"]);
}
