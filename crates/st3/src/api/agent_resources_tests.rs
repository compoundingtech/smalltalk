use super::*;

fn append(store: &Store, subject: &str, kind: &str, fields: Value) -> ClaimRecord {
    store.append_claim(&ClaimInput {
        subject: subject.into(), kind: kind.into(), actor: Some(subject.into()),
        fields: serde_json::from_value(fields).unwrap(), evidence: Vec::new(),
        expected_subject: None, idempotency_key: None,
    }).unwrap()
}

fn declare(state: &AppState) {
    let source = "version 2\nagent \"amber\" { command \"true\" }\nagent \"cobalt\" { command \"true\" }\nagent \"indigo\" { command \"true\" }\n";
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let plan = state.store.mission(&intent, IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    state.store.apply(&intent, &plan.subject_tokens, "filtered-frontier").unwrap();
}

fn timeline(store: &Store, subject: &str) -> ClaimRecord {
    append(store, subject, "harness.timeline", json!({
        "operation":"append", "entry_id":"filtered-frontier-entry", "revision":1,
        "role":"assistant", "entry_type":"content", "final":true,
        "body":{"media_type":"text/plain", "text":"Local activity"},
        "driver":"codex", "incarnation_id":"one", "sequence":1,
    }))
}

#[test]
fn message_commit_rebuilds_only_sender_and_recipient_cards() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::state(root.path());
    declare(&state);
    let store = &state.store;
    for history in [false, true] {
        client_agent_resources_cached(store, history, store.index().unwrap()).unwrap();
    }
    let message = append(store, "message/filtered-frontier", "message.sent", json!({
        "from":"agent/node.amber", "to":"agent/node.cobalt", "content":"Activity",
    }));
    for history in [false, true] {
        let cards = store.read_snapshot(|index| {
            store.cached_agent_resources(index, history, |changed| {
                let (subjects, previous) = changed.expect("message must not refill the fleet");
                assert_eq!(subjects, &BTreeSet::from([
                    "agent/node.amber".into(), "agent/node.cobalt".into(),
                ]));
                let cards = client_agent_resources_selected(store, history, index, changed)?;
                assert_eq!(cards.len(), 2);
                assert!(previous.iter().find(|card| card["id"] == "agent/node.indigo")
                    .unwrap()["last_activity_at"].is_null());
                Ok(cards)
            })
        }).unwrap();
        for id in ["agent/node.amber", "agent/node.cobalt"] {
            assert_eq!(cards.iter().find(|card| card["id"] == id).unwrap()["last_activity_at"],
                json!(client_timestamp(message.accepted_at_unix_ms)));
        }
        assert!(cards.iter().find(|card| card["id"] == "agent/node.indigo")
            .unwrap()["last_activity_at"].is_null());
    }
}

#[test]
fn agent_card_frontier_comes_from_the_pinned_sqlite_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let mut state = tests::state(root.path());
    state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
    declare(&state);
    let store = &state.store;
    append(store, "agent/node.cobalt", "runtime.observed", json!({
        "status":"running", "runtime_id":"cobalt", "incarnation_id":"one",
    }));
    append(store, "agent/node.cobalt", "harness.observed", json!({
        "state":"working", "driver":"codex", "incarnation_id":"one",
    }));
    let before = store.read_snapshot(|index| client_agent_resources_cached(store, false, index)).unwrap();
    store.read_snapshot(|index| {
        let writer = store.clone();
        std::thread::spawn(move || {
            let record = timeline(&writer, "agent/node.cobalt");
            assert!(record.id.starts_with("local-observation/"));
            assert_eq!(writer.index().unwrap(), index);
        }).join().unwrap();
        assert_eq!(client_agent_resources_cached(store, false, index)?, before,
            "a newer writer frontier must not enter this old read snapshot");
        Ok(())
    }).unwrap();
    let after = store.read_snapshot(|index| client_agent_resources_cached(store, false, index)).unwrap();
    let cobalt = after.iter().find(|card| card["id"] == "agent/node.cobalt").unwrap();
    assert!(!cobalt["last_activity_at"].is_null());
    assert!(!cobalt["silent_since"].is_null());
    assert_eq!(after.iter().find(|card| card["id"] == "agent/node.indigo"),
        before.iter().find(|card| card["id"] == "agent/node.indigo"));
}

#[tokio::test]
async fn filtered_agents_refresh_local_observations_and_keep_frozen_cursors() {
    let root = tempfile::tempdir().unwrap();
    let state = tests::state(root.path());
    declare(&state);
    for seat in ["amber", "cobalt", "indigo"] {
        let subject = format!("agent/node.{seat}");
        append(&state.store, &subject, "runtime.observed", json!({
            "status":"running", "runtime_id":seat, "incarnation_id":"one",
        }));
        append(&state.store, &subject, "harness.observed", json!({
            "state":if seat == "amber" { "idle" } else { "indeterminate" },
            "driver":"codex", "incarnation_id":"one",
        }));
    }
    let app = router(state.clone());
    let (status, first) = tests::get_request(app.clone(), "/v1/client/agents?status=waiting&limit=1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["value"]["items"][0]["id"], "agent/node.cobalt",
        "filtering must exclude amber before applying limit=1");
    assert_eq!(first["value"]["page"]["has_more"], true);
    let index = state.store.index().unwrap();
    let observation = timeline(&state.store, "agent/node.cobalt");
    timeline(&state.store, "agent/node.indigo");
    assert_eq!(state.store.index().unwrap(), index);
    let (_, fresh) = tests::get_request(app.clone(), "/v1/client/agents?status=waiting&limit=1").await;
    assert_eq!(fresh["value"]["items"][0]["last_activity_at"],
        json!(client_timestamp(observation.accepted_at_unix_ms)));
    assert!(first["value"]["items"][0]["last_activity_at"].is_null());
    let cursor = first["value"]["page"]["next_cursor"].as_str().unwrap();
    let (status, second) = tests::get_request(app,
        &format!("/v1/client/agents?status=waiting&limit=1&cursor={cursor}")).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["value"]["items"][0]["id"], "agent/node.indigo");
    assert!(second["value"]["items"][0]["last_activity_at"].is_null(),
        "continuations must retain their first page's pre-observation cards");
    assert_eq!(second["value"]["page"]["has_more"], false);
}
