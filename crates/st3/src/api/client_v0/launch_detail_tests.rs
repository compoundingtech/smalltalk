use super::super::*;
use smallclaims::sqlite::work::SqliteWorkScope;
use std::sync::Barrier;

fn fixture(root: &Path) -> AppState {
    super::tests::test_state_named(root, "launch-detail")
}

fn session(state: &AppState, id: &str) {
    state
        .store
        .create_planning_session(
            id,
            "detail",
            "doc/detail/request@missing",
            "/tmp/detail",
            "person/ada",
            "agent/detail/planner",
            &PlannerSpec::default(),
            Some("mission-run/detail/source"),
            Some("run-generation/detail/source"),
        )
        .unwrap();
}

fn rich_session(state: &AppState, id: &str) {
    session(state, id);
    let document = state
        .store
        .put_document(
            "doc/detail/request",
            b"Inspect the selected launch.",
            &None,
            "detail-request",
        )
        .unwrap();
    // Real selected session, candidate and preview projections; no runtime is launched.
    let source = r#"version 2
mission "detail" state="ready" {
 goal "Inspect the selected launch."
 step "inspect" { goal "Inspect."; assigned-to "agent/detail/worker" }
}
"#;
    let (_, preview) = mission_source(state, source, None).unwrap();
    let request = format!("{}@{}", document.name, document.hash);
    let connection = rusqlite::Connection::open(state.state_dir.join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    connection
        .execute(
            "UPDATE planning_sessions SET request_ref=?1 WHERE id=?2",
            rusqlite::params![request, id],
        )
        .unwrap();
    for variant in ["default", "other"] {
        let view = state
            .store
            .add_planning_candidate(
                id,
                "agent/detail/planner",
                variant,
                "doc/detail/markdown@missing",
                &format!("doc/detail/kdl@{variant}"),
                "detail-revision",
            )
            .unwrap();
        let revision = view
            .variants
            .iter()
            .find(|item| item.name == variant)
            .unwrap()
            .candidate
            .revision;
        state
            .store
            .save_planning_preview(
                id,
                variant,
                revision,
                "detail-preview",
                "graph",
                "diff",
                &preview,
            )
            .unwrap();
    }
}

async fn get(state: &AppState, id: &str, history: bool) -> (StatusCode, Value) {
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/v1/client/launches/{}?history={history}",
                    urlencoding::encode(id)
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Retained collection-first selection, including native-ID priority before alias lookup.
fn oracle(state: &AppState, id: &str, history: bool) -> Option<Value> {
    let items = client_launch_resources(state, history).unwrap();
    let native = format!("launch/{id}");
    let selected = if items.iter().any(|item| item["id"] == native) {
        &native
    } else {
        id
    };
    client_detail(items, "launch", selected)
        .ok()
        .map(|value| value.0)
}

#[tokio::test]
async fn launch_detail_preserves_native_ids_history_fields_and_scope() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    rich_session(&state, "one");
    for id in [
        "launch/one",
        "planning-session/one",
        "cancelled",
        "approved",
        "failed",
    ] {
        session(&state, id);
    }
    let connection = rusqlite::Connection::open(root.path().join("graph.db")).unwrap();
    crate::store::configure_projection_writer(&connection).unwrap();
    for status in ["cancelled", "approved", "failed"] {
        connection
            .execute(
                "UPDATE planning_sessions SET status=?1 WHERE id=?1",
                [status],
            )
            .unwrap();
    }
    for history in [false, true] {
        for id in [
            "one",
            "launch/one",
            "launch/launch/one",
            "planning-session/one",
            "launch/planning-session/one",
            "cancelled",
            "approved",
            "failed",
            "missing",
        ] {
            let expected = state
                .store
                .read_snapshot(|_| Ok(oracle(&state, id, history)))
                .unwrap();
            let (status, value) = get(&state, id, history).await;
            assert_eq!(
                status,
                if expected.is_some() {
                    StatusCode::OK
                } else {
                    StatusCode::NOT_FOUND
                },
                "{id}: {value}"
            );
            if let Some(expected) = expected {
                assert_eq!(value["value"], expected, "{id}");
                assert_eq!(
                    value["snapshot"]["store_index"],
                    state.store.index().unwrap()
                );
            } else {
                assert_eq!(value["code"], "not-found");
                assert!(value.get("value").is_none());
            }
        }
    }
    let (_, rich) = get(&state, "one", false).await;
    assert_eq!(
        rich["value"]["target"]["generation_id"],
        "run-generation/detail/source"
    );
    assert_eq!(rich["value"]["variants"].as_array().unwrap().len(), 2);
    assert!(rich["value"]["preview_token"].is_string());
    assert_eq!(
        rich["value"]["preview"]["request_excerpt"],
        "Inspect the selected launch."
    );
    // A historical native ID does not mask a visible public-ID fallback.
    state
        .store
        .finish_planning_session("launch/one", "person/ada", "cancelled", None)
        .unwrap();
    let (_, current) = get(&state, "launch/one", false).await;
    assert_eq!(current["value"]["id"], "launch/one");
    let (_, historical) = get(&state, "launch/one", true).await;
    assert_eq!(historical["value"]["id"], "launch/launch/one");
    assert_eq!(historical["value"]["phase"], "cancelled");
    // Scope is checked before selection, including missing IDs.
    let mut caller = super::ClientSession::local(None).unwrap();
    caller.scopes.clear();
    let error = client_launches_detail(
        State(state.clone()),
        Extension(caller),
        AxumPath("missing".into()),
        Query(ClientListQuery::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, StatusCode::FORBIDDEN);
}

// Nested handler reads reuse the caller's reader on the daemon's multithread runtime.
// A current-thread runtime deliberately dispatches them to another worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_detail_and_envelope_use_one_cut_across_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture(root.path());
    rich_session(&state, "one");
    let barrier = Arc::new(Barrier::new(2));
    let writer_state = state.clone();
    let writer_barrier = barrier.clone();
    let writer = std::thread::spawn(move || {
        writer_barrier.wait();
        writer_state
            .store
            .finish_planning_session("one", "person/ada", "cancelled", None)
            .unwrap();
        writer_state
            .store
            .put_document(
                "doc/detail/concurrent",
                b"after",
                &None,
                "detail-concurrent",
            )
            .unwrap();
        writer_barrier.wait();
    });
    let reader_state = state.clone();
    let before = read_deadline::spawn_handler(move || {
        reader_state.store.read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader_state, index);
            let expected = client_launch_detail_at(&reader_state, "one", false)?.unwrap();
            barrier.wait();
            barrier.wait();
            // Invoke the real handler inside an already pinned reader, so item and response
            // extension must both retain the cut despite the admitted writer completing.
            let caller = super::ClientSession::local(None).unwrap();
            let (Extension(envelope), Json(item)) = tokio::runtime::Handle::current()
                .block_on(client_launches_detail(
                    State(reader_state.clone()),
                    Extension(caller),
                    AxumPath("one".into()),
                    Query(ClientListQuery::default()),
                ))
                .unwrap();
            assert_eq!(item, expected);
            assert_eq!(envelope.id, snapshot.id);
            assert_eq!(envelope.store_index, index);
            Ok(snapshot)
        })
    })
    .await
    .unwrap()
    .unwrap();
    writer.join().unwrap();
    let (status, after) = get(&state, "one", false).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{after}");
    let (status, after) = get(&state, "one", true).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["value"]["phase"], "cancelled");
    assert!(after["snapshot"]["store_index"].as_u64().unwrap() > before.store_index);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_detail_selected_work_stays_flat_as_unrelated_sessions_and_declarations_grow() {
    let mut selected_before = None;
    let mut oracle_before = None;
    for count in [32, 320] {
        let root = tempfile::tempdir().unwrap();
        let state = fixture(root.path());
        let mut declarations = String::from(
            "version 2\nagent \"detail/worker\" { workspace \"/tmp/detail\"; harness \"codex\" {} }\n",
        );
        for number in 0..count {
            declarations.push_str(&format!(
                "agent \"other/{number}\" {{ workspace \"/tmp/other\"; harness \"codex\" {{}} }}\n"
            ));
            session(&state, &format!("unrelated-{number}"));
        }
        let intent = crate::graph::parse_intent(&declarations, &state.node).unwrap();
        state
            .store
            .apply_internal(&intent, "detail-growth")
            .unwrap();
        rich_session(&state, "one");
        // Measure the real handler's SQLite work on its pinned thread, separately from
        // process-global counters and authentication. The router is also exercised below.
        let reader_state = state.clone();
        let (selected, cost) = read_deadline::spawn_handler(move || {
            let scope = SqliteWorkScope::start();
            let caller = super::ClientSession::local(None).unwrap();
            let (_, Json(selected)) = tokio::runtime::Handle::current()
                .block_on(client_launches_detail(
                    State(reader_state),
                    Extension(caller),
                    AxumPath("one".into()),
                    Query(ClientListQuery::default()),
                ))
                .unwrap();
            (selected, scope.finish())
        })
        .await
        .unwrap();
        let (expected, old_cost) = state
            .store
            .read_snapshot(|_| {
                let scope = SqliteWorkScope::start();
                let expected = oracle(&state, "one", false).unwrap();
                Ok((expected, scope.finish()))
            })
            .unwrap();
        assert_eq!(selected, expected);
        assert_eq!(selected["preview"]["agents"].as_array().unwrap().len(), 1);
        assert_eq!(
            selected["preview"]["agents"][0]["id"],
            "agent/detail/worker"
        );
        let (status, routed) = get(&state, "one", false).await;
        assert_eq!(status, StatusCode::OK, "{routed}");
        assert_eq!(routed["value"], selected);
        assert!(
            cost.statements > 0 && cost.vm_steps > 0,
            "positive measured work: {cost:?}"
        );
        if let Some(prior) = selected_before {
            let prior: smallclaims::sqlite::work::SqliteWork = prior;
            assert_eq!(cost.statements, prior.statements);
            assert_eq!(cost.fullscan_steps, prior.fullscan_steps);
            assert_eq!(cost.sorts, prior.sorts);
            assert_eq!(cost.autoindex_rows, prior.autoindex_rows);
            assert!(
                cost.vm_steps <= prior.vm_steps + 64,
                "{prior:?} -> {cost:?}"
            );
            let old: smallclaims::sqlite::work::SqliteWork = oracle_before.unwrap();
            assert!(
                old_cost.vm_steps > old.vm_steps * 3,
                "old collection negative: {old:?} -> {old_cost:?}"
            );
        }
        selected_before = Some(cost);
        oracle_before = Some(old_cost);
        eprintln!("launch-detail unrelated={count} selected={cost:?} old-list={old_cost:?}");
    }
}
