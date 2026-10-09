//! Attention windows served from the published attention list: each session sees exactly what
//! the full computation shows it at the publication's cut, reads never fold the list, and a
//! window before the first publication is a retryable refusal, never an empty list.
use super::*;

fn custom(state: &AppState, subject: &str, person: &str) {
    let manifest =
        serde_json::from_str(include_str!("../../../../../examples/st3/custom-review.json"))
            .unwrap();
    state
        .store
        .register_custom_kind(&crate::store::custom::RegistrationRequest {
            manifest,
            actor: "agent/garden/seed".into(),
        })
        .unwrap();
    state
        .store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "custom.garden.review.v1.requested".into(),
            actor: Some("agent/garden/seed".into()),
            fields: serde_json::from_value(
                json!({"title":"Choose a seed","detail":"Keep or discard","recipient":person}),
            )
            .unwrap(),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

async fn window(
    state: &AppState,
    session: &ClientSession,
    person: Option<&str>,
    limit: usize,
) -> Result<(ClientSnapshot, Vec<Value>, bool), ApiError> {
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe", "id":"attention", "collection":"attention",
        "limit":limit, "person":person,
    }))
    .unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap();
    collection_items(state, session, &request, permit).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attention_windows_serve_the_published_list_to_each_session_and_never_fold() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "attention-list");
    custom(&state, "custom/garden/review/v1/ada-one", "person/ada");
    custom(&state, "custom/garden/review/v1/ada-two", "person/ada");
    custom(&state, "custom/garden/review/v1/robin", "person/robin");
    // A store whose refresher has not published yet refuses the window, retryably.
    state.store.start_attention_list_refresher().unwrap();
    let refused = window(&state, &ClientSession::local(Some("person/ada")).unwrap(), None, 50)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "attention-not-ready");
    assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    crate::api::refresh_attention_list(&state.store).unwrap();
    let publication = state.store.newest_attention_list().1.unwrap();
    let full = |person: Option<&str>| {
        let mut rows = crate::api::client_attention_resources_at(
            &state.store,
            person,
            false,
            publication.evaluated_at_unix_ms,
        )
        .unwrap();
        client_attention_compatibility(&mut rows, false);
        rows
    };
    let folds = state.store.attention_list_folds();
    for (session, person, expected) in [
        // A person sees their own rows, whatever they ask for nothing else.
        ("person/ada", None, full(Some("person/ada"))),
        ("person/robin", Some("person/robin"), full(Some("person/robin"))),
        // An agent sees every person's rows, or the person it selects.
        ("agent/garden/seed", None, full(None)),
        ("agent/garden/seed", Some("person/ada"), full(Some("person/ada"))),
        ("agent/garden/seed", Some("person/visitor"), Vec::new()),
    ] {
        let client = ClientSession::local(Some(session)).unwrap();
        let (snapshot, items, has_more) = window(&state, &client, person, 50).await.unwrap();
        assert_eq!(snapshot.store_index, publication.cut);
        assert!(snapshot.published_at.is_some(), "a served list says when it is from");
        assert!(!has_more);
        assert_eq!(items, expected, "{session:?} {person:?}");
    }
    assert_eq!(full(None).len(), 3);
    // A person cannot select another person's private rows.
    let other = window(&state, &ClientSession::local(Some("person/ada")).unwrap(), Some("person/robin"), 50)
        .await
        .unwrap_err();
    assert_eq!(other.status, StatusCode::FORBIDDEN);
    // A bounded window keeps the order and says there is more.
    let (_, items, has_more) =
        window(&state, &ClientSession::local(Some("agent/garden/seed")).unwrap(), None, 2)
            .await
            .unwrap();
    assert_eq!(items, full(None)[..2]);
    assert!(has_more);
    assert_eq!(state.store.attention_list_folds(), folds, "windows never fold");
    // A window behind a newer cut serves the publication's own cut until the refresher runs.
    custom(&state, "custom/garden/review/v1/ada-three", "person/ada");
    let ada = ClientSession::local(Some("person/ada")).unwrap();
    let (stale, items, _) = window(&state, &ada, None, 50).await.unwrap();
    assert_eq!(stale.store_index, publication.cut);
    assert_eq!(items.len(), 2);
    crate::api::refresh_attention_list(&state.store).unwrap();
    let (fresh, items, _) = window(&state, &ada, None, 50).await.unwrap();
    assert_eq!(fresh.store_index, state.store.index().unwrap());
    assert_eq!(items.len(), 3);
    // Once its refresher stops, a window reads for itself at its own cut again.
    custom(&state, "custom/garden/review/v1/ada-four", "person/ada");
    state.store.stop_attention_list_refresher();
    let (own, items, _) = window(&state, &ada, None, 50).await.unwrap();
    assert_eq!(own.store_index, state.store.index().unwrap());
    assert!(own.published_at.is_none());
    assert_eq!(items.len(), 4);
}

async fn summary_window(
    state: &AppState,
    session: &ClientSession,
) -> Result<(ClientSnapshot, Vec<Value>, bool), ApiError> {
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe", "id":"top-bar", "collection":"summary", "limit":1,
    }))
    .unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap();
    collection_items(state, session, &request, permit).await
}

/// The counts a summary window computes for itself at the current cut.
fn computed_summary(state: &AppState, session: &ClientSession) -> Value {
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe", "id":"top-bar", "collection":"summary", "limit":1,
    }))
    .unwrap();
    state
        .store
        .read_snapshot(|index| {
            summary::native(
                state,
                session,
                &request,
                &client_snapshot_at(state, index),
                client_now_ms(),
                None,
                None,
            )
        })
        .unwrap()
        .remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn summary_windows_serve_the_published_row_of_their_selection() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "summary-list");
    custom(&state, "custom/garden/review/v1/ada-one", "person/ada");
    custom(&state, "custom/garden/review/v1/robin", "person/robin");
    state.store.start_attention_list_refresher().unwrap();
    let ada = ClientSession::local(Some("person/ada")).unwrap();
    let agent = ClientSession::local(Some("agent/garden/seed")).unwrap();
    // Before its selection is published a window is refused, retryably, and names it.
    for session in [&ada, &agent] {
        let refused = summary_window(&state, session).await.unwrap_err();
        assert_eq!(refused.code, "summary-not-ready");
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    assert!(summary::refresh_published(&state, 60_000).unwrap());
    let cut = state.store.index().unwrap();
    let counts = |row: &Value| {
        json!([row["person_id"], row["needs_you"], row["working_agents"],
            row["active_missions"], row["machines"], row["revision"]])
    };
    for session in [&ada, &agent] {
        let (snapshot, items, has_more) = summary_window(&state, session).await.unwrap();
        assert_eq!(snapshot.store_index, cut);
        assert!(snapshot.published_at.is_some());
        assert!(!has_more);
        assert_eq!(items.len(), 1);
        assert_eq!(counts(&items[0]), counts(&computed_summary(&state, session)));
    }
    let (_, items, _) = summary_window(&state, &ada).await.unwrap();
    assert_eq!(items[0]["needs_you"], 1, "Ada's own request");
    let (_, items, _) = summary_window(&state, &agent).await.unwrap();
    assert_eq!(items[0]["needs_you"], 2, "every person's");
    // Equal counts publish nothing new; a new request for Ada does.
    assert!(!summary::refresh_published(&state, 60_000).unwrap());
    custom(&state, "custom/garden/review/v1/ada-two", "person/ada");
    assert!(summary::refresh_published(&state, 60_000).unwrap());
    let (_, items, _) = summary_window(&state, &ada).await.unwrap();
    assert_eq!(items[0]["needs_you"], 2);
    assert_eq!(counts(&items[0]), counts(&computed_summary(&state, &ada)));
}
