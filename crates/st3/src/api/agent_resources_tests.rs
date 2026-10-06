use super::*;
use super::tests::{get_request, state};

fn append(store: &Store, subject: &str, kind: &str, fields: Value) {
    store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields: serde_json::from_value(fields).unwrap(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
    // Raw graph appends do not eagerly refresh the work projection used by card queues.
    store.project_replication_backlog().unwrap();
}

fn declare(store: &Store, names: &[&str]) {
    let source = format!(
        "version 2\n{}",
        names
            .iter()
            .map(|name| format!("agent \"{name}\" {{ command \"true\" }}\n"))
            .collect::<String>()
    );
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let planned = store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: source,
                source_name: None,
            },
        )
        .unwrap();
    store
        .apply(&intent, &planned.subject_tokens, &format!("projection-test-roster-{}", store.index().unwrap()))
        .unwrap();
}

// Compare the cache's complete card contract, including todo and its identity fence.
// Request-time clock/presence overlays are deliberately outside this cache contract.
fn project(
    store: &Store,
    history: bool,
    index: u64,
    changed: Option<AgentResourceDelta<'_>>,
) -> anyhow::Result<Vec<Value>> {
    let mut cards = client_agent_resources_selected(store, history, index, changed)?;
    let subjects = cards
        .iter()
        .map(|card| card["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let observations = store.agent_todo_observations_for(&subjects, index)?;
    for card in &mut cards {
        let claims = observations.get(card["id"].as_str().unwrap());
        card["todo"] = client_v0::agent_todo_value(
            claims.and_then(|claims| claims.get("harness.todo.observed")),
            claims.and_then(|claims| claims.get("harness.session-file")),
            card["incarnation_id"].as_str(),
        );
    }
    Ok(cards)
}

fn checked_cards(store: &Store, history: bool) -> Vec<Value> {
    store.project_replication_backlog().unwrap();
    let index = store.index().unwrap();
    let cached = store
        .cached_agent_resources(index, history, |changed| {
            project(store, history, index, changed)
        })
        .unwrap();
    let full = project(store, history, index, None).unwrap();
    assert!(
        serde_json::to_vec(&cached).unwrap() == serde_json::to_vec(&full).unwrap(),
        "card bytes differ at index {index}, history={history}: cached={cached:?}, full={full:?}"
    );
    cached
}

fn todo(incarnation: &str, session: &str, sequence: usize) -> Value {
    json!({
        "harness":"omp", "session_id":session, "incarnation_id":incarnation,
        "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
        "phases":[{"name":"Build","tasks":[{
            "content":format!("Deploy {sequence}"), "status":"blocked", "blocker":"Approval"
        }]}],
        "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":1,"abandoned":0},
        "truncated":false
    })
}

#[test]
fn deterministic_commit_sequences_match_full_card_bytes_including_todo() {
    for seed in [1_u64, 0x138, 0xdead_beef, 0x9e37_79b9] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = &state.store;
        declare(store, &["amber", "birch", "cobalt", "dune"]);
        for name in ["amber", "birch", "cobalt", "dune"] {
            let subject = format!("agent/node.{name}");
            append(store, &subject, "runtime.observed", json!({
                "status":"running", "runtime_id":name, "incarnation_id":"one"
            }));
            append(store, &subject, "harness.session-file", json!({
                "harness":"omp", "agent":subject, "session_id":"native-one", "path":"/tmp/unused-session"
            }));
            append(store, &subject, "harness.todo.observed", todo("one", "native-one", 0));
        }
        for history in [false, true] {
            let cards = checked_cards(store, history);
            assert!(cards.iter().all(|card| card["todo"]["stale"] == false));
        }
        let mut random = seed;
        for sequence in 0..64 {
            random = random.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let name = ["amber", "birch", "cobalt", "dune"][(random >> 32) as usize % 4];
            let subject = format!("agent/node.{name}");
            let incarnation = if random & 1 == 0 { "one" } else { "two" };
            match (random >> 16) % 6 {
                0 => append(store, &subject, "runtime.observed", json!({
                    "status":if random & 2 == 0 { "running" } else { "failed" },
                    "runtime_id":name, "incarnation_id":incarnation
                })),
                1 => append(store, &subject, "harness.observed", json!({
                    "state":if random & 2 == 0 { "idle" } else { "working" },
                    "driver":"omp", "incarnation_id":incarnation,
                    "provider_auth":random & 4 == 0
                })),
                2 => append(store, &subject, "harness.todo.observed", todo(incarnation, "native-one", sequence)),
                3 => append(store, &subject, "harness.session-file", json!({
                    "harness":"omp", "agent":subject,
                    "session_id":if random & 2 == 0 { "native-one" } else { "native-two" },
                    "path":"/tmp/unused-session"
                })),
                4 => append(store, &subject, "harness.todo.observed", json!({
                    "harness":"omp", "session_id":"native-one", "incarnation_id":incarnation,
                    "observed_at":"2026-10-03T09:00:00Z", "source_op":"clear",
                    "phases":[], "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0,"abandoned":0},
                    "truncated":false
                })),
                _ => append(store, "resource/unrelated", "resource.observed", json!({"kind":"vcs.pull-request", "facts":{"number":sequence}})),
            }
            for history in [false, true] {
                checked_cards(store, history);
            }
        }
    }
}

#[test]
fn isolated_agent_change_reduces_only_that_agent_and_retains_other_cards() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    declare(store, &["amber", "birch", "cobalt", "dune"]);
    let before = checked_cards(store, false);
    let fills = store.agent_resources_full_fills();
    append(store, "agent/node.cobalt", "runtime.observed", json!({
        "status":"running", "runtime_id":"cobalt", "incarnation_id":"one"
    }));
    let index = store.index().unwrap();
    let mut reductions = Vec::new();
    let cards = store.cached_agent_resources(index, false, |changed| {
        let delta = changed.expect("isolated change must not rebuild the roster");
        let subjects = delta.subjects;
        let previous = delta.previous;
        reductions.push(subjects.clone());
        assert_eq!(subjects, &BTreeSet::from(["agent/node.cobalt".to_owned()]));
        assert_eq!(previous.iter().map(|card| card["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["agent/node.cobalt"]);
        project(store, false, index, changed)
    }).unwrap();
    assert_eq!(reductions, vec![BTreeSet::from(["agent/node.cobalt".to_owned()])]);
    assert_eq!(store.agent_resources_full_fills(), fills);
    assert_eq!(serde_json::to_vec(&cards).unwrap(), serde_json::to_vec(&project(store, false, index, None).unwrap()).unwrap());
    for old in before.iter().filter(|card| card["id"] != "agent/node.cobalt") {
        assert_eq!(cards.iter().find(|card| card["id"] == old["id"]).unwrap(), old);
    }
}

#[test]
fn unknown_kind_and_schema_mismatch_fall_back_to_full_projection() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    declare(store, &["amber", "birch"]);
    checked_cards(store, false);
    let mut fills = store.agent_resources_full_fills();
    for invalidate_schema in [false, true] {
        if invalidate_schema {
            store.invalidate_agent_resources_schema();
        } else {
            append(store, "agent/node.amber", "runtime.observed", json!({
                "status":"running", "runtime_id":"amber", "incarnation_id":"future"
            }));
            // Model a claim retained from a newer writer without weakening public admission.
            // This mutation touches only this isolated in-memory fixture's newest claim.
            let inserted_index = store.index().unwrap();
            store.connection.write().execute(
                "UPDATE claims SET kind='future.agent.observed' WHERE store_index=?1",
                [inserted_index],
            ).unwrap();
        }
        let index = store.index().unwrap();
        let mut calls = 0;
        let cards = store.cached_agent_resources(index, false, |changed| {
            assert!(changed.is_none(), "unknown kind/schema mismatch requires a complete reduction");
            calls += 1;
            project(store, false, index, changed)
        }).unwrap();
        assert_eq!(calls, 1);
        assert_eq!(store.agent_resources_full_fills(), fills + 1);
        fills += 1;
        assert_eq!(serde_json::to_vec(&cards).unwrap(), serde_json::to_vec(&project(store, false, index, None).unwrap()).unwrap());
    }
}

#[tokio::test]
async fn limit_one_http_pages_keep_full_roster_order_after_incremental_updates() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    declare(&state.store, &["dune", "amber", "cobalt", "birch"]);
    let expected = checked_cards(&state.store, false).iter()
        .map(|card| card["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(expected, vec!["agent/node.amber", "agent/node.birch", "agent/node.cobalt", "agent/node.dune"]);
    append(&state.store, "agent/node.cobalt", "runtime.observed", json!({
        "status":"running", "runtime_id":"cobalt", "incarnation_id":"one"
    }));
    let app = router(state.clone());
    let mut received = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let path = format!("/v1/client/agents?limit=1{}", cursor.as_deref()
            .map(|cursor| format!("&cursor={}", urlencoding::encode(cursor))).unwrap_or_default());
        let (status, page) = get_request(app.clone(), &path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        received.push(items[0]["id"].as_str().unwrap().to_owned());
        assert!(received.len() <= expected.len(), "pagination repeated roster rows");
        cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
        // A later commit must not reorder or replace the cursor's roster snapshot.
        append(&state.store, "agent/node.amber", "harness.observed", json!({
            "state":"working", "driver":"omp", "incarnation_id":"one"
        }));
    }
    assert_eq!(received, expected);
}

fn card<'a>(cards: &'a [Value], subject: &str) -> &'a Value {
    cards.iter().find(|card| card["id"] == subject).unwrap()
}

fn check_both_histories(store: &Store) -> Vec<Value> {
    let current = checked_cards(store, false);
    checked_cards(store, true);
    current
}

#[test]
fn messages_and_declaration_renames_refresh_dependencies_and_roster_order() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    declare(store, &["amber", "birch"]);
    for subject in ["agent/node.amber", "agent/node.birch"] {
        append(store, subject, "runtime.observed", json!({
            "status":"running", "runtime_id":subject, "incarnation_id":"one"
        }));
        append(store, subject, "harness.observed", json!({
            "state":"working", "driver":"omp", "incarnation_id":"one"
        }));
    }
    let before = check_both_histories(store);
    let fills = store.agent_resources_full_fills();
    append(store, "message/dependency", "message.sent", json!({
        "from":"agent/node.amber", "to":"agent/node.birch",
        "status":"sent", "content":"Coordinate this work."
    }));
    // Give the message a fixture-controlled time later than both working edges, so the
    // silent-since transition is deterministic even on a clock with millisecond resolution.
    let message_index = store.index().unwrap();
    store.connection.write().execute(
        "UPDATE claims SET accepted_at_unix_ms=?1 WHERE store_index=?2",
        rusqlite::params![(client_now_ms() + 60_000).to_string(), message_index],
    ).unwrap();
    let after = check_both_histories(store);
    assert_eq!(store.agent_resources_full_fills(), fills);
    for subject in ["agent/node.amber", "agent/node.birch"] {
        assert_ne!(card(&after, subject)["last_activity_at"], card(&before, subject)["last_activity_at"]);
        assert!(card(&after, subject)["last_activity_at"].is_string());
        assert_ne!(card(&after, subject)["silent_since"], card(&before, subject)["silent_since"]);
        assert_eq!(card(&after, subject)["silent_since"], card(&after, subject)["last_activity_at"]);
    }
    store.rename_agent("agent/node.birch", Some("Aardvark"), "rename-projection").unwrap();
    let renamed = check_both_histories(store);
    assert_eq!(renamed[0]["id"], "agent/node.birch");
    assert_eq!(renamed[0]["name"], "Aardvark");
    declare(store, &["amber", "birch", "cobalt"]);
    let declared = check_both_histories(store);
    assert!(declared.iter().any(|card| card["id"] == "agent/node.cobalt"));
    let stop = crate::graph::parse_test_intent("version 2\nstop \"agent/node.amber\"\n", "node").unwrap();
    store.apply_internal(&stop, "retire-projection-seat").unwrap();
    check_both_histories(store);
    append(store, "agent/node.amber", "runtime.observed", json!({
        "status":"stopped", "runtime_id":"amber", "incarnation_id":"retired"
    }));
    let removed = check_both_histories(store);
    assert!(!removed.iter().any(|card| card["id"] == "agent/node.amber"));
    assert!(checked_cards(store, true).iter().any(|card| card["id"] == "agent/node.amber"));
}

#[test]
fn queue_move_handoff_and_terminal_transitions_refresh_old_and_new_participants() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = r#"
version 2
agent "amber" { command "true" }
agent "birch" { command "true" }
mission "dependencies" state="ready" {
  concurrent-runs max=2
  goal "Exercise queue dependencies."
  step "build" { title "Build"; assigned-to "agent/node.amber" }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store.mission(&intent, crate::model::IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "queue-dependencies").unwrap();
    check_both_histories(store);
    let create = |key: &str| store.create_mission_run(&MissionRunRequest {
        mission:"dependencies".into(), revision:None,
        workspace:root.path().display().to_string(), requester:Some("person/test".into()),
        mode:Some("run".into()), inputs:BTreeMap::new(), idempotency_key:key.into(),
    }).unwrap();
    let first = create("dependency-first");
    check_both_histories(store);
    let second = create("dependency-second");
    check_both_histories(store);
    let first_step = &first.steps[0].subject;
    let second_step = &second.steps[0].subject;
    for step in [first_step, second_step] {
        store.set_step_state(step, "ready", None).unwrap();
        check_both_histories(store);
    }
    let before_move = check_both_histories(store);
    assert_eq!(card(&before_move, "agent/node.amber")["next_work_id"], *first_step);
    store.move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
        agent:"agent/node.amber".into(), run:second.subject.clone(), placement:"top".into(),
        anchor:None, reason:None, actor:"person/test".into(), idempotency_key:"dependency-move".into(),
    }).unwrap();
    let moved = check_both_histories(store);
    assert_eq!(card(&moved, "agent/node.amber")["next_work_id"], *second_step);
    append(store, second_step, "work.claimed", json!({
        "status":"claimed", "attempt":1, "claimant":"agent/node.amber",
        "claim_incarnation":"one", "claim_expires_at_unix_ms":client_now_ms() + 60_000
    }));
    let claimed = check_both_histories(store);
    assert_eq!(card(&claimed, "agent/node.amber")["current_work_ids"], json!([second_step]));
    append(store, second_step, "work.released", json!({
        "status":"ready", "attempt":1, "handoff_to":"agent/node.birch"
    }));
    let handed_off = check_both_histories(store);
    let amber = card(&handed_off, "agent/node.amber");
    let birch = card(&handed_off, "agent/node.birch");
    assert_eq!(amber["current_work_ids"], json!([]));
    assert_eq!(amber["next_work_id"], *first_step);
    assert!(!amber["upcoming_work_ids"].as_array().unwrap().contains(&json!(second_step)));
    assert_eq!(birch["next_work_id"], *second_step);
    store.set_step_state(second_step, "completed", None).unwrap();
    let completed = check_both_histories(store);
    assert!(card(&completed, "agent/node.birch")["next_work_id"].is_null());
    append(store, &first.generation, "run-generation.state", json!({
        "status":"cancelled", "phase":"terminal", "reason":"Generation ended."
    }));
    check_both_histories(store);
    append(store, &first.subject, "mission-run.state", json!({
        "status":"cancelled", "phase":"terminal", "reason":"Run ended."
    }));
    let run_ended = check_both_histories(store);
    for subject in ["agent/node.amber", "agent/node.birch"] {
        assert_eq!(card(&run_ended, subject)["current_work_ids"], json!([]));
        assert_eq!(card(&run_ended, subject)["upcoming_work_ids"], json!([]));
    }
}

#[test]
fn same_claim_index_local_timeline_advances_card_without_full_fill() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    declare(store, &["amber", "birch"]);
    append(store, "agent/node.amber", "runtime.observed", json!({
        "status":"running", "runtime_id":"amber", "incarnation_id":"one"
    }));
    append(store, "agent/node.amber", "harness.observed", json!({
        "state":"working", "driver":"omp", "incarnation_id":"one"
    }));
    let before = check_both_histories(store);
    assert!(card(&before, "agent/node.amber")["last_activity_at"].is_null());
    let index = store.index().unwrap();
    let fills = store.agent_resources_full_fills();
    append(store, "agent/node.amber", "harness.timeline", json!({
        "operation":"append", "entry_id":"local-message", "source_id":"source/local-message",
        "sequence":1, "revision":1, "role":"assistant", "entry_type":"message",
        "final":true, "driver":"omp", "incarnation_id":"one",
        "body":{"text":"Substantive progress"}
    }));
    assert_eq!(store.index().unwrap(), index, "timeline is a local observation, not a new claim");
    for history in [false, true] {
        let mut calls = 0;
        let cards = store.cached_agent_resources(index, history, |changed| {
            let delta = changed.expect("local advancement must reduce only the affected seat");
            let subjects = delta.subjects;
            let previous = delta.previous;
            assert_eq!(subjects, &BTreeSet::from(["agent/node.amber".to_owned()]));
            assert_eq!(previous.len(), 1);
            calls += 1;
            project(store, history, index, changed)
        }).unwrap();
        assert_eq!(calls, 1, "same-index hit must observe the new local frontier");
        assert_eq!(serde_json::to_vec(&cards).unwrap(), serde_json::to_vec(&project(store, history, index, None).unwrap()).unwrap());
        assert!(card(&cards, "agent/node.amber")["last_activity_at"].is_string());
        assert_eq!(card(&cards, "agent/node.birch"), card(&before, "agent/node.birch"));
    }
    assert_eq!(store.agent_resources_full_fills(), fills);
}

#[test]
fn run_and_generation_terminal_edges_remove_owned_seats_from_current_roster() {
    for terminate_generation in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = &state.store;
        let source = r#"
version 2
mission "owned-dependency" state="ready" {
  goal "Fence owned seats at their owner's terminal edge."
  agent "worker" { workspace "/tmp"; harness "codex" {} }
  step "build" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = crate::graph::parse_test_intent(source, "node").unwrap();
        let planned = store.mission(&intent, crate::model::IntentInput {
            kdl:source.into(), source_name:None,
        }).unwrap();
        store.apply(&intent, &planned.subject_tokens, "owned-dependency").unwrap();
        let run = store.create_mission_run(&MissionRunRequest {
            mission:"owned-dependency".into(), revision:None, workspace:root.path().display().to_string(),
            requester:Some("person/test".into()), mode:Some("run".into()),
            inputs:BTreeMap::new(), idempotency_key:"owned-dependency-run".into(),
        }).unwrap();
        let mission = store.mission_spec("owned-dependency", Some(&run.revision)).unwrap().unwrap();
        let mut owned = crate::graph::parse_execution_intent(
            mission.declarations_kdl.as_deref().unwrap(), "node", &run.id,
        ).unwrap();
        let subject = format!("agent/{}/worker", run.id);
        let desired = owned.subjects.get_mut(&subject).unwrap();
        desired.owner_run = Some(run.subject.clone());
        desired.owner_generation = Some(run.generation.clone());
        store.apply_internal(&owned, "materialize-owned-dependency").unwrap();
        assert_eq!(card(&check_both_histories(store), &subject)["operational"]["layer"], "current");
        let (owner, kind) = if terminate_generation {
            (&run.generation, "run-generation.state")
        } else {
            (&run.subject, "mission-run.state")
        };
        append(store, owner, kind, json!({
            "status":"cancelled", "phase":"terminal", "reason":"Owner ended."
        }));
        let current = check_both_histories(store);
        assert!(!current.iter().any(|card| card["id"] == subject));
        let history = checked_cards(store, true);
        assert_eq!(card(&history, &subject)["operational"]["layer"], "history");
    }
}

#[test]
fn handoff_beyond_queue_preview_refreshes_initial_assignee_total() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let steps = (0..8)
        .map(|number| format!("  step \"build-{number}\" {{ assigned-to \"agent/node.amber\" }}\n"))
        .collect::<String>();
    let source = format!(
        "version 2\nagent \"amber\" {{ command \"true\" }}\nagent \"birch\" {{ command \"true\" }}\n\
         mission \"preview-handoff\" state=\"ready\" {{\n  goal \"Transfer work outside the card preview.\"\n{steps}}}\n"
    );
    let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
    let planned = store.mission(&intent, crate::model::IntentInput {
        kdl: source, source_name: None,
    }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "preview-handoff-source").unwrap();
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "preview-handoff".into(), revision: None,
        workspace: root.path().display().to_string(), requester: Some("person/test".into()),
        mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: "preview-handoff-run".into(),
    }).unwrap();
    for step in &run.steps {
        store.set_step_state(&step.subject, "ready", None).unwrap();
    }
    let before = check_both_histories(store);
    let amber = card(&before, "agent/node.amber");
    assert_eq!(amber["queued_work_count"], 8);
    assert_eq!(card(&before, "agent/node.birch")["queued_work_count"], 0);
    let omitted = run.steps.iter().find(|step| {
        amber["next_work_id"] != step.subject
            && !amber["upcoming_work_ids"].as_array().unwrap().contains(&json!(step.subject))
            && !amber["current_work_ids"].as_array().unwrap().contains(&json!(step.subject))
    }).expect("eight ready steps must include a step outside the bounded preview");
    assert!(store.claims_for(&omitted.subject, Some("work.claimed")).unwrap().is_empty());
    let fills = store.agent_resources_full_fills();
    append(store, &omitted.subject, "work.released", json!({
        "status": "ready", "attempt": 1, "handoff_to": "agent/node.birch"
    }));
    let after = check_both_histories(store);
    assert_eq!(card(&after, "agent/node.amber")["queued_work_count"], 7);
    assert_eq!(card(&after, "agent/node.birch")["queued_work_count"], 1);
    assert_eq!(card(&after, "agent/node.birch")["next_work_id"], omitted.subject);
    assert_eq!(store.agent_resources_full_fills(), fills);
}

#[test]
fn message_reference_to_absent_agent_does_not_create_history_card() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    declare(store, &["amber"]);
    check_both_histories(store);
    let fills = store.agent_resources_full_fills();
    append(store, "message/absent-recipient", "message.sent", json!({
        "from": "agent/node.amber", "to": "agent/absent",
        "status": "sent", "content": "A party reference is not an agent observation."
    }));
    for history in [false, true] {
        let cards = checked_cards(store, history);
        assert_eq!(cards.iter().map(|card| card["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["agent/node.amber"]);
        assert!(card(&cards, "agent/node.amber")["last_activity_at"].is_string());
        assert!(cards.iter().all(|card| card["id"] != "agent/absent"));
    }
    assert_eq!(store.agent_resources_full_fills(), fills);
}

#[test]
fn message_after_work_lease_expiry_matches_full_queue_reduction() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let store = &state.store;
    let source = r#"
version 2
agent "amber" { command "true" }
mission "lease-expiry" state="ready" {
  goal "Exercise time-derived queue invalidation."
  step "build" { assigned-to "agent/node.amber" }
}
"#;
    let intent = crate::graph::parse_test_intent(source, "node").unwrap();
    let planned = store.mission(&intent, crate::model::IntentInput {
        kdl: source.into(), source_name: None,
    }).unwrap();
    store.apply(&intent, &planned.subject_tokens, "lease-expiry-source").unwrap();
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "lease-expiry".into(), revision: None,
        workspace: root.path().display().to_string(), requester: Some("person/test".into()),
        mode: Some("run".into()), inputs: BTreeMap::new(), idempotency_key: "lease-expiry-run".into(),
    }).unwrap();
    let step = &run.steps[0].subject;
    store.set_step_state(step, "ready", None).unwrap();
    store.project_replication_backlog().unwrap();
    let expiry = client_now_ms() + 2_000;
    append(store, step, "work.claimed", json!({
        "status": "claimed", "attempt": 1, "claimant": "agent/node.amber",
        "claim_incarnation": "one", "claim_expires_at_unix_ms": expiry
    }));
    let before = check_both_histories(store);
    assert_eq!(card(&before, "agent/node.amber")["current_work_ids"], json!([step]));
    let fills = store.agent_resources_full_fills();
    // Wait for the actual reducer clock boundary, not an assumed scheduling delay.
    while client_now_ms() < expiry {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    append(store, "message/after-lease-expiry", "message.sent", json!({
        "from": "agent/node.amber", "to": "person/test", "status": "sent",
        "content": "The lease expired without a work claim."
    }));
    let after = check_both_histories(store);
    assert_eq!(card(&after, "agent/node.amber")["current_work_ids"], json!([]));
    assert_eq!(card(&after, "agent/node.amber")["next_work_id"], *step);
    assert_eq!(store.agent_resources_full_fills(), fills + 2);
}
