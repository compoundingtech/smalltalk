use super::*;
use crate::harness_events::Publication;
const SEAT: &str = "agent/example/event-seat";
fn runtime(store: &Store, incarnation: &str, status: &str) {
    store
        .append_claim(&ClaimInput {
            subject: SEAT.into(),
            kind: "runtime.observed".into(),
            actor: Some(SEAT.into()),
            fields: BTreeMap::from([
                ("status".into(), json!(status)),
                ("incarnation_id".into(), json!(incarnation)),
            ]),
            evidence: vec![],
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}
fn event(kind: &str) -> Publication {
    let fields = match kind {
        "harness.observed" => BTreeMap::from([
            ("state".into(), json!("working")),
            ("driver".into(), json!("claude")),
            ("incarnation_id".into(), json!("runtime-a")),
        ]),
        "harness.usage" => BTreeMap::from([
            ("semantics".into(), json!("context_occupancy")),
            ("context_used_tokens".into(), json!(10)),
            ("driver".into(), json!("claude")),
            ("incarnation_id".into(), json!("runtime-a")),
        ]),
        "harness.limits" => BTreeMap::from([
            ("driver".into(), json!("claude")),
            ("measured_at_unix_ms".into(), json!(1)),
            ("incarnation_id".into(), json!("runtime-a")),
            ("five_hour_percent".into(), json!(5.0)),
        ]),
        "harness.timeline" => BTreeMap::from([
            ("operation".into(), json!("append")),
            ("entry_id".into(), json!("entry-a")),
            ("sequence".into(), json!(1)),
            ("revision".into(), json!(1)),
            ("role".into(), json!("assistant")),
            ("entry_type".into(), json!("content")),
            ("final".into(), json!(true)),
            ("body".into(), json!({"text":"hello"})),
            ("driver".into(), json!("claude")),
            ("incarnation_id".into(), json!("runtime-a")),
            ("observed_at_unix_ms".into(), json!(1)),
        ]),
        _ => unreachable!(),
    };
    Publication {
        runtime_incarnation: "runtime-a".into(),
        sequence: 1,
        claim: ClaimInput {
            subject: SEAT.into(),
            kind: kind.into(),
            actor: Some(SEAT.into()),
            fields,
            evidence: vec![],
            expected_subject: None,
            idempotency_key: Some("producer-event".into()),
        },
    }
}
#[test]
fn harness_todo_event_replay_and_runtime_fencing_preserve_latest_snapshot() {
    let store = Store::open_memory("amber").unwrap();
    runtime(&store, "runtime-a", "running");
    let mut input = event("harness.observed");
    input.claim.kind = "harness.todo.observed".into();
    input.claim.fields = serde_json::from_value(json!({
        "harness":"omp","session_id":"native-a","incarnation_id":"runtime-a",
        "observed_at":"2026-10-03T15:00:00Z","source_op":"block",
        "phases":[{"name":"Review","tasks":[
            {"content":"Review claim","status":"blocked","blocker":"Await review"}
        ]}],"totals":{"pending":0,"in_progress":0,"completed":0,"blocked":1},
        "truncated":false
    })).unwrap();
    let (first, changed) = store.append_harness_event(&input).unwrap();
    assert!(changed);
    let (replay, changed) = store.append_harness_event(&input).unwrap();
    assert!(!changed);
    assert_eq!(replay.body["fields"], first.body["fields"]);
    let original = input.claim.fields.clone();
    input.claim.fields.insert("source_op".into(), json!("hydrate"));
    assert_eq!(store.append_harness_event(&input).unwrap_err().code, "idempotency-mismatch");
    input.claim.fields = original;
    input.sequence = 2;
    input.claim.fields.insert("phases".into(), json!([]));
    input.claim.fields.insert("totals".into(), json!({
        "pending":0,"in_progress":0,"completed":0,"blocked":0
    }));
    input.claim.fields.insert("source_op".into(), json!("clear"));
    let (cleared, changed) = store.append_harness_event(&input).unwrap();
    assert!(changed);
    assert_eq!(cleared.body["fields"]["phases"], json!([]));
    runtime(&store, "runtime-b", "running");
    input.sequence = 3;
    assert_eq!(store.append_harness_event(&input).unwrap_err().code, "stale-harness-event-session");
    let current = store.latest_observation(SEAT, "harness.todo.observed").unwrap().unwrap();
    assert_eq!(current.body["fields"]["source_op"], "clear");
    const FLEET: &str = "018f6f0d-4a5d-7b8c-9d0e-123456789abc";
    let peer = Store::open_memory("blue").unwrap();
    store.bind_fleet(FLEET).unwrap();
    peer.bind_fleet(FLEET).unwrap();
    let exchange = store.export_replication_exchange(
        FLEET, &ReplicationInventory::default(),
    ).unwrap();
    peer.receive_replication_exchange("amber", FLEET, &exchange).unwrap();
    peer.validate_replication_backlog().unwrap();
    peer.apply_replication_repairs().unwrap();
    assert!(peer.project_replication_backlog().unwrap());
    let remote = peer.agent_todo_observations_for(
        &[SEAT.to_owned()], peer.index().unwrap(),
    ).unwrap();
    let remote = &remote[SEAT]["harness.todo.observed"];
    assert_eq!(remote.id, cleared.id);
    assert_eq!(remote.body["fields"]["source_op"], "clear");
}
#[test]
fn native_event_admission_fences_every_retention_and_replays_lost_acknowledgements() {
    for kind in [
        "harness.observed",
        "harness.usage",
        "harness.limits",
        "harness.timeline",
    ] {
        let store = Store::open_memory("amber").unwrap();
        let mut input = event(kind);
        assert_eq!(
            store.append_harness_event(&input).unwrap_err().code,
            "stale-harness-event-session"
        );
        runtime(&store, "runtime-a", "running");
        let (first, changed) = store.append_harness_event(&input).unwrap();
        assert!(changed);
        let (replayed, changed) = store.append_harness_event(&input).unwrap();
        assert!(!changed);
        if !latest_retention(kind) {
            assert_eq!(first.id, replayed.id);
        }
        assert_eq!(replayed.body["fields"], json!(input.claim.fields));
        input.claim.fields.insert("driver".into(), json!("codex"));
        assert_eq!(
            store.append_harness_event(&input).unwrap_err().code,
            "idempotency-mismatch"
        );
        input.claim.fields.insert("driver".into(), json!("claude"));
        runtime(&store, "runtime-b", "running");
        assert_eq!(
            store.append_harness_event(&input).unwrap_err().code,
            "stale-harness-event-session"
        );
        input.runtime_incarnation = "runtime-b".into();
        let (replayed, changed) = store.append_harness_event(&input).unwrap();
        assert!(!changed);
        if !latest_retention(kind) {
            assert_eq!(first.id, replayed.id);
        }
        input.sequence = 2;
        let (historical, _) = store.append_harness_event(&input).unwrap();
        assert_eq!(
            historical
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str),
            input
                .claim
                .fields
                .get("incarnation_id")
                .and_then(Value::as_str)
        );
        runtime(&store, "runtime-b", "exited");
        input.sequence = 3;
        assert_eq!(
            store.append_harness_event(&input).unwrap_err().code,
            "stale-harness-event-session"
        );
    }
}
#[test]
fn terminal_state_can_be_followed_by_its_final_timeline_before_runtime_exit() {
    let store = Store::open_memory("amber").unwrap();
    runtime(&store, "runtime-a", "running");
    let mut state = event("harness.observed");
    state.claim.fields.insert("state".into(), json!("ended"));
    store.append_harness_event(&state).unwrap();
    let mut timeline = event("harness.timeline");
    timeline.sequence = 2;
    store.append_harness_event(&timeline).unwrap();
    runtime(&store, "runtime-a", "exited");
    timeline.sequence = 3;
    assert!(store.append_harness_event(&timeline).is_err());
}
#[test]
fn event_envelopes_cannot_publish_foreign_or_non_harness_claims() {
    let store = Store::open_memory("amber").unwrap();
    runtime(&store, "runtime-a", "running");
    let mut input = event("harness.observed");
    input.claim.actor = Some("agent/example/other".into());
    assert_eq!(
        store.append_harness_event(&input).unwrap_err().code,
        "invalid-harness-event"
    );
    input.claim.actor = Some(SEAT.into());
    input.sequence = 0;
    assert!(store.append_harness_event(&input).is_err());
    input.sequence = 1;
    input.claim.kind = "intent.desired".into();
    assert!(store.append_harness_event(&input).is_err());
}
