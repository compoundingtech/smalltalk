use super::*;

fn sent_messages(state: &AppState, accepted_at: u128) {
    state.store.set_write_clock_at(accepted_at).unwrap();
    for index in 0..3 {
        state.store.append_claim(&ClaimInput {
            subject: format!("message/cursor-binding-{index}"),
            kind: "message.sent".into(),
            actor: Some("person/sender".into()),
            fields: BTreeMap::from([
                ("from".into(), json!("person/sender")),
                ("to".into(), json!("person/recipient")),
                ("content".into(), json!(format!("cursor binding body {index}"))),
                ("status".into(), json!("sent")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        }).unwrap();
    }
}

async fn first_page(state: &AppState, now: u128) -> (ClientSnapshot, ClientListQuery, String) {
    let query = ClientListQuery { history: true, limit: Some(1), ..Default::default() };
    let (Extension(snapshot), Json(page)) = client_messages_sql_page_at(
        state, new_client_snapshot(state), &query, now,
    ).await.unwrap();
    let cursor = page.page.next_cursor.expect("fixture spans pages");
    (snapshot, query, cursor)
}

async fn assert_cursor_gone(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
    now: u128,
) {
    let error = client_messages_sql_page_at(state, snapshot, query, now)
        .await.expect_err("unissued or unavailable cut must be rejected");
    assert_eq!(error.status, StatusCode::GONE);
    assert_eq!(error.code, "page-cursor-expired");
}

#[tokio::test]
async fn message_cursor_rejects_forged_before_index_without_leaking_later_writes() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now.saturating_sub(60_000));
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let expected = client_message_resources_at(&state.store, None, true, None, now).unwrap();
    state.store.append_claim(&ClaimInput {
        subject: "message/cursor-binding-1".into(),
        kind: "message.read".into(),
        actor: Some("person/recipient".into()),
        fields: BTreeMap::from([("status".into(), json!("read"))]),
        evidence: Vec::new(), expected_subject: None, idempotency_key: None,
    }).unwrap();
    let mut forged = decode_client_cursor(&cursor).unwrap();
    let newer_index = state.store.index().unwrap();
    assert!(newer_index > forged.before_index.unwrap());
    forged.before_index = Some(newer_index);
    let forged_query = ClientListQuery {
        cursor: Some(encode_client_cursor(&forged).unwrap()), ..query.clone()
    };
    assert_cursor_gone(&state, snapshot.clone(), &forged_query, now).await;
    let legitimate = ClientListQuery { cursor: Some(cursor), ..query };
    let (_, Json(page)) = client_messages_sql_page_at(&state, snapshot, &legitimate, now + 60_000)
        .await.unwrap();
    assert_eq!(page.items, expected[1..2]);
}

#[tokio::test]
async fn message_cursor_rejects_forged_expiry_and_uses_retained_first_request_age() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now.saturating_sub(60_000));
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let expected = client_message_resources_at(&state.store, None, true, None, now).unwrap();
    for delta in [1, CLIENT_PAGE_TTL_MS] {
        let mut forged = decode_client_cursor(&cursor).unwrap();
        forged.expires_at_unix_ms += delta;
        let forged_query = ClientListQuery {
            cursor: Some(encode_client_cursor(&forged).unwrap()), ..query.clone()
        };
        assert_cursor_gone(&state, snapshot.clone(), &forged_query, now).await;
    }
    let legitimate = ClientListQuery { cursor: Some(cursor), ..query };
    let (_, Json(page)) = client_messages_sql_page_at(&state, snapshot, &legitimate, now + 60_000)
        .await.unwrap();
    assert_eq!(page.items, expected[1..2]);
}

#[tokio::test]
async fn message_cursor_binds_every_issued_page_position_and_snapshot_field() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now);
    let (_, query, cursor) = first_page(&state, now).await;
    let issued = decode_client_cursor(&cursor).unwrap();
    let mut forged = vec![issued.clone(); 7];
    forged[0].offset += 1;
    forged[1].after_key.as_mut().unwrap().0 += 1;
    forged[2].after_key.as_mut().unwrap().1.push_str("-forged");
    forged[3].items_digest = format!("message-cut/{}", uuid::Uuid::now_v7());
    forged[4].snapshot.created_at = client_timestamp(now + 1);
    forged[5].snapshot.host_id.push_str("-forged");
    forged[6].snapshot.projection_version.push_str("-forged");
    for forged in forged {
        let forged_query = ClientListQuery {
            cursor: Some(encode_client_cursor(&forged).unwrap()), ..query.clone()
        };
        // Middleware takes a local cursor's snapshot from the cursor itself. Even
        // matching that supplied snapshot cannot substitute the issued metadata.
        assert_cursor_gone(&state, forged.snapshot, &forged_query, now).await;
    }
}

#[tokio::test]
async fn message_cursor_returns_gone_when_retained_metadata_is_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now);
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let receipt = decode_client_cursor(&cursor).unwrap().items_digest;
    // Remove only this receipt, avoiding interference with concurrent API tests.
    CLIENT_MESSAGE_CUTS.lock().retain(|cut| cut.cursor.items_digest != receipt);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    assert_cursor_gone(&state, snapshot, &continuation, now).await;
}


#[tokio::test]
async fn message_cursor_expires_at_the_server_issued_deadline() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now);
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let expiry = decode_client_cursor(&cursor).unwrap().expires_at_unix_ms;
    assert_eq!(expiry, now + CLIENT_PAGE_TTL_MS);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    assert_cursor_gone(&state, snapshot, &continuation, expiry).await;
}

#[tokio::test]
async fn message_cursor_cannot_cross_stores_with_the_same_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let first = super::tests::state(root.path());
    let other = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&first, now);
    sent_messages(&other, now);
    let (snapshot, query, cursor) = first_page(&first, now).await;
    assert_eq!(new_client_snapshot(&other), snapshot);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    assert_cursor_gone(&other, snapshot, &continuation, now).await;
}

#[tokio::test]
async fn message_cursor_is_invalidated_by_claim_deletion() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let now = client_now_ms();
    sent_messages(&state, now);
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let epoch = state.store.client_messages_cut_epoch().unwrap();
    let removed = state.store.connection.batched(|tx| {
        tx.execute("DELETE FROM claims WHERE subject='message/cursor-binding-1'", [])
    }).unwrap().unwrap();
    assert_eq!(removed, 1);
    assert!(state.store.client_messages_cut_epoch().unwrap() > epoch);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    assert_cursor_gone(&state, snapshot, &continuation, now).await;
    let store = Arc::downgrade(&state.store);
    assert!(!CLIENT_MESSAGE_CUTS.lock().iter().any(|cut|cut.store.ptr_eq(&store) && cut.cut_epoch==epoch));
}

#[tokio::test]
async fn message_cursor_is_invalidated_by_desired_deletion() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::state(root.path());
    let intent = crate::graph::parse_test_intent(r#"version 2
agent "cursor-legacy" { workspace "/tmp"; command "true" }
message "cursor-legacy-0" { from "requester"; to "cursor-legacy"; content "zero" }
message "cursor-legacy-1" { from "requester"; to "cursor-legacy"; content "one" }
message "cursor-legacy-2" { from "requester"; to "cursor-legacy"; content "two" }
"#, "node").unwrap();
    let mission = state.store.mission(&intent, crate::model::IntentInput {
        kdl: "cursor-desired-deletion".into(), source_name: None,
    }).unwrap();
    state.store.apply(&intent, &mission.subject_tokens, "cursor-desired-deletion").unwrap();
    let now = client_now_ms();
    let (snapshot, query, cursor) = first_page(&state, now).await;
    let epoch = state.store.client_messages_cut_epoch().unwrap();
    let removed = state.store.connection.batched(|tx| {
        tx.execute("DELETE FROM desired WHERE subject='message/cursor-legacy-1'", [])
    }).unwrap().unwrap();
    assert_eq!(removed, 1);
    assert!(state.store.client_messages_cut_epoch().unwrap() > epoch);
    let continuation = ClientListQuery { cursor: Some(cursor), ..query };
    assert_cursor_gone(&state, snapshot, &continuation, now).await;
    let store = Arc::downgrade(&state.store);
    assert!(!CLIENT_MESSAGE_CUTS.lock().iter().any(|cut|cut.store.ptr_eq(&store) && cut.cut_epoch==epoch));
}
