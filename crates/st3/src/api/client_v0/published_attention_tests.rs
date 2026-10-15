//! Attention windows served from the published attention list: each session sees exactly what
//! the full computation shows it at the publication's cut, reads never fold the list, and a
//! window before the first publication is a retryable refusal, never an empty list.
use super::*;

fn custom(state: &AppState, subject: &str, person: &str) {
    let manifest =
        serde_json::from_str(/* sqlite-large-value: allow-for-now lv-haa0cb564a8c1 owner=st3::api::client_v0::published_attention_tests TODO=trace-statement-and-value-representation */ include_str!("../../../../../examples/st3/custom-review.json"))
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
    summary_window_for(state, session, None).await
}

async fn summary_window_for(
    state: &AppState,
    session: &ClientSession,
    person: Option<&str>,
) -> Result<(ClientSnapshot, Vec<Value>, bool), ApiError> {
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe", "id":"top-bar", "collection":"summary", "limit":1, "person":person,
    }))
    .unwrap();
    let permit = Arc::new(tokio::sync::Semaphore::new(1)).acquire_owned().await.unwrap();
    collection_items(state, session, &request, permit).await
}

/// The counts a summary window computes for itself at the current cut.
fn computed_summary(state: &AppState, session: &ClientSession) -> Value {
    computed_summary_for(state, session, None)
}

fn computed_summary_for(state: &AppState, session: &ClientSession, person: Option<&str>) -> Value {
    let request: CollectionSubscribe = serde_json::from_value(json!({
        "kind":"subscribe", "id":"top-bar", "collection":"summary", "limit":1, "person":person,
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
    // Before anything is published a window is refused, retryably, and names it.
    for session in [&ada, &agent] {
        let refused = summary_window(&state, session).await.unwrap_err();
        assert_eq!(refused.code, "summary-not-ready");
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    // Shared counts alone count no person's alerts: windows wait for attention as well.
    assert!(!summary::refresh_published(&state, 60_000, false).unwrap());
    assert!(state.store.summary_shared().is_some());
    assert_eq!(summary_window(&state, &ada).await.unwrap_err().code, "summary-not-ready");
    crate::api::refresh_attention_list(&state.store).unwrap();
    assert!(summary::refresh_published(&state, 60_000, false).unwrap());
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
    // Equal counts publish nothing new; a new request for Ada does, once attention has it.
    assert!(!summary::refresh_published(&state, 60_000, false).unwrap());
    custom(&state, "custom/garden/review/v1/ada-two", "person/ada");
    crate::api::refresh_attention_list(&state.store).unwrap();
    assert!(summary::refresh_published(&state, 60_000, false).unwrap());
    let (_, items, _) = summary_window(&state, &ada).await.unwrap();
    assert_eq!(items[0]["needs_you"], 2);
    assert_eq!(counts(&items[0]), counts(&computed_summary(&state, &ada)));
    // A window that opens a new selection is served at once, from the published shared counts
    // and attention rows, before the refresher publishes that selection.
    let robin = ClientSession::local(Some("person/robin")).unwrap();
    let (snapshot, items, _) = summary_window(&state, &robin).await.unwrap();
    assert!(snapshot.published_at.is_some());
    assert_eq!(snapshot.store_index, state.store.summary_shared().unwrap().0.cut);
    assert_eq!(counts(&items[0]), counts(&computed_summary(&state, &robin)));
    assert!(state.store.summary_selection_waiting());
    assert!(summary::refresh_published(&state, 60_000, true).unwrap(), "Robin's row is published");
    assert!(!state.store.summary_selection_waiting());
    // A waiting refresh computes no shared counts again: it only publishes the waiting rows.
    custom(&state, "custom/garden/review/v1/ada-three", "person/ada");
    crate::api::refresh_attention_list(&state.store).unwrap();
    let held = state.store.summary_shared().unwrap().0;
    assert!(!summary::refresh_published(&state, 60_000, true).unwrap());
    assert!(Arc::ptr_eq(&held, &state.store.summary_shared().unwrap().0));
    let (_, items, _) = summary_window(&state, &ada).await.unwrap();
    assert_eq!(items[0]["needs_you"], 2, "published again only by a full refresh");
    assert!(summary::refresh_published(&state, 60_000, false).unwrap());
    let (_, items, _) = summary_window(&state, &ada).await.unwrap();
    assert_eq!(items[0]["needs_you"], 3);
    assert_eq!(counts(&items[0]), counts(&computed_summary(&state, &ada)));
}

/// The published counts of a selection are the counts its window would compute for itself:
/// alerts from the published attention rows equal those from the attention items, for each
/// person and for every person, and an update is no alert.
#[test]
fn published_attention_alerts_are_the_native_alerts() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "summary-counts");
    custom(&state, "custom/garden/review/v1/ada", "person/ada");
    custom(&state, "custom/garden/review/v1/robin", "person/robin");
    let now = client_now_ms();
    let mut rows = crate::api::client_attention_resources_at(&state.store, None, false, now).unwrap();
    for person in [None, Some("person/ada"), Some("person/robin"), Some("person/visitor")] {
        let items = state.store.attention_snapshot(person, now).unwrap();
        let native = items.iter().filter(|item| item.is_alert()).count();
        assert_eq!(summary::published_attention_alerts(&rows, person), native, "{person:?}");
    }
    rows.push(json!({"person_id":"person/ada","alert":false,"attention_kind":"person-step"}));
    assert_eq!(summary::published_attention_alerts(&rows, Some("person/ada")), 1);
}

/// As the daemon starts the refresher computes the counts every summary shares with no window
/// reading, so the first window of any selection is served at once and never waits for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_summary_window_of_any_selection_is_served_at_once() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "summary-start");
    custom(&state, "custom/garden/review/v1/ada", "person/ada");
    crate::api::start_attention_list(&state);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while state.store.summary_shared().is_none() || state.store.newest_attention_list().1.is_none() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for the start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!state.store.summary_shared_starting());
    for (session, person) in [("person/ada", None), ("agent/garden/seed", None),
        ("agent/garden/seed", Some("person/robin"))]
    {
        let session = ClientSession::local(Some(session)).unwrap();
        let (snapshot, items, _) = summary_window_for(&state, &session, person).await.unwrap();
        assert!(snapshot.published_at.is_some());
        let computed = computed_summary_for(&state, &session, person);
        for field in ["person_id", "needs_you", "active_missions", "machines", "revision"] {
            assert_eq!(items[0][field], computed[field], "{field}");
        }
    }
    state.store.stop_attention_list_refresher();
}

/// The shared counts read the complete agents roster. As the daemon starts it may not be
/// published yet; a window then waits for that publication, not for the refresher's pause.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_summary_waiting_for_the_roster_is_served_once_it_publishes() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "summary-roster");
    custom(&state, "custom/garden/review/v1/ada", "person/ada");
    // A roster refresher that has published nothing yet: no summary can be counted.
    let _roster = state.store.start_agent_roster_refresher().unwrap();
    crate::api::start_attention_list(&state);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while state.store.newest_attention_list().1.is_none() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for attention");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ada = ClientSession::local(Some("person/ada")).unwrap();
    assert_eq!(summary_window(&state, &ada).await.unwrap_err().code, "summary-not-ready");
    // Well inside the refresher's pause of at least a second after its first refresh.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(state.store.summary_shared().is_none());
    assert!(state.store.summary_shared_starting(), "nothing was counted yet");
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || crate::api::refresh_agent_roster(&store, false))
        .await
        .unwrap()
        .unwrap();
    let published = std::time::Instant::now();
    let items = loop {
        if let Ok((snapshot, items, _)) = summary_window(&state, &ada).await {
            assert!(snapshot.published_at.is_some());
            break items;
        }
        assert!(
            published.elapsed() < Duration::from_millis(500),
            "the summary waited past the roster's publication"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(!state.store.summary_shared_starting());
    let computed = computed_summary(&state, &ada);
    for field in ["person_id", "needs_you", "working_agents", "active_missions", "machines", "revision"] {
        assert_eq!(items[0][field], computed[field], "{field}");
    }
    state.store.stop_attention_list_refresher();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn odd_and_surplus_summary_selections_never_stop_the_others() {
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "summary-selections");
    custom(&state, "custom/garden/review/v1/ada-one", "person/ada");
    state.store.start_attention_list_refresher().unwrap();
    let agent = ClientSession::local(Some("agent/garden/seed")).unwrap();
    let counts = |row: &Value| {
        json!([row["person_id"], row["needs_you"], row["working_agents"],
            row["active_missions"], row["machines"], row["revision"]])
    };
    // An agent may select any name; one that is no person still publishes, beside the others.
    for person in ["not-a-person", "person/ada"] {
        assert!(summary_window_for(&state, &agent, Some(person)).await.is_err());
    }
    crate::api::refresh_attention_list(&state.store).unwrap();
    summary::refresh_published(&state, 60_000, false).unwrap();
    for person in ["not-a-person", "person/ada"] {
        let (snapshot, items, _) = summary_window_for(&state, &agent, Some(person)).await.unwrap();
        assert!(snapshot.published_at.is_some(), "{person} is published");
        assert_eq!(counts(&items[0]), counts(&computed_summary_for(&state, &agent, Some(person))));
    }
    // Unchanged inputs compute nothing again.
    assert!(!summary::refresh_published(&state, 60_000, false).unwrap());
    // Past the selections it keeps, a window reads its own summary at once.
    for number in 0..64 {
        let _ = state.store.published_summary(Some(&format!("person/{number}")));
    }
    let surplus = "person/robin";
    let (snapshot, items, _) = summary_window_for(&state, &agent, Some(surplus)).await.unwrap();
    assert!(snapshot.published_at.is_none(), "read by the window itself");
    assert_eq!(counts(&items[0]), counts(&computed_summary_for(&state, &agent, Some(surplus))));
}

/// The running refresher after views are forgotten (a checkpoint, a chunk of a long catch-up):
/// with no window reading, a pass publishes glasses again but does not fold attention, so a
/// node nobody reads never refolds it once per forget. The next attention window is refused,
/// retryably, and its read wakes the refresher, which folds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_forget_attention_folds_again_only_once_a_window_reads_it() {
    use crate::store::owner_lists::OwnerView;
    let root = tempfile::tempdir().unwrap();
    let state = super::tests::test_state_named(root.path(), "forget-gate");
    custom(&state, "custom/garden/review/v1/ada", "person/ada");
    let glass = |id: usize, name: &str| ClaimInput {
        subject: format!("glass/person/ada/019a0000-0000-7000-8000-{id:012x}"),
        kind: "glass.upserted".into(),
        actor: Some("person/ada".into()),
        fields: serde_json::from_value(json!({
            "body":{"name":name,"layout":{"tabs":[{"pane":"opaque:anything"}]}},
            "base_revision":null,
        }))
        .unwrap(),
        evidence: vec![],
        expected_subject: None,
        idempotency_key: None,
    };
    state.store.append_claim(&glass(1, "Desk")).unwrap();
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    crate::api::start_attention_list(&state);
    // As the daemon starts, every view is published with no window reading.
    until("the first publication", &|| {
        state.store.newest_attention_list().1.is_some()
            && state.store.owner_list_cut(OwnerView::Glasses).is_some()
    });
    assert!(!state.store.attention_list_read_within(u64::MAX));
    state.store.forget_current_views();
    assert!(state.store.newest_attention_list().1.is_none());
    assert!(state.store.owner_list_cut(OwnerView::Glasses).is_none());
    // A commit wakes the refresher: glasses are published again, after attention in the same
    // pass, and attention is not folded.
    state.store.append_claim(&glass(2, "Window")).unwrap();
    state.event_notify.send_modify(|generation| *generation += 1);
    until("glasses again", &|| state.store.owner_list_cut(OwnerView::Glasses).is_some());
    assert!(state.store.newest_attention_list().1.is_none(), "no window reads attention");
    // A window's read is refused, retryably, and wakes the refresher, which folds it.
    let ada = ClientSession::local(Some("person/ada")).unwrap();
    let refused = window(&state, &ada, None, 50).await.unwrap_err();
    assert_eq!(refused.code, "attention-not-ready");
    until("attention again", &|| state.store.newest_attention_list().1.is_some());
    let (snapshot, items, _) = window(&state, &ada, None, 50).await.unwrap();
    assert!(snapshot.published_at.is_some());
    assert_eq!(items.len(), 1);
    state.store.stop_attention_list_refresher();
}
