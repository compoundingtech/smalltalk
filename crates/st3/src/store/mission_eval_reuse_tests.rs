use super::*;

struct Clock;
impl Clock {
    fn at(at: u128) -> Self {
        smallclaims::store::set_thread_clock(Some(at));
        Self
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        smallclaims::store::set_thread_clock(None);
    }
}

// Freeze the old query path here rather than comparing two aliases of the optimized loader.
fn legacy_run(connection: &Connection, id: &str, snapshot: u128) -> rusqlite::Result<MissionRunView> {
    let mut view = mission_run_header_tx(connection, id)?;
    let mut statement = connection.prepare(
        "SELECT subject, run_id, step_path, definition_hash, status, attempt, assignee, available_to, agentless, title, goals, worker_reported,
                lease_owner, lease_incarnation, lease_expires_at_unix_ms, blocked_reason, not_before_unix_ms, created_at_unix_ms, updated_at_unix_ms, readiness_epoch, constraints
         FROM step_runs WHERE generation_id=?1 ORDER BY created_at_unix_ms, step_path",
    )?;
    view.steps = statement.query_map([generation_id_from_subject(&view.generation)], step_run_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for step in &mut view.steps {
        enrich_step_queue_for_reconcile_at(connection, step, snapshot)?;
        let (started, elapsed) = step_execution_timing_at(connection, &step.subject, step.attempt, snapshot,
            matches!(step.status.as_str(), "claimed" | "working"))?;
        step.execution_started_at_unix_ms = started;
        step.execution_elapsed_ms = elapsed;
        step.timeout_extension_ms = step_timeout_extension_at(connection, &step.subject, step.attempt, snapshot)?;
    }
    view.provenance = crate::provenance::read(connection, &view.mission, &view.revision)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
    view.loops = loop_run_views_tx(connection, &view)?;
    view.outcome = mission_run_outcome_tx(connection, &view)?;
    Ok(view)
}

fn assert_equivalent(store: &Store, id: &str, snapshot: u128) {
    let _clock = Clock::at(snapshot);
    let expected = {
        let connection = store.readers.get();
        legacy_run(&connection, id, snapshot).unwrap()
    };
    let (actual, mission) = store.mission_run_for_evaluation(id).unwrap();
    assert_eq!(serde_json::to_value(&actual).unwrap(), serde_json::to_value(&expected).unwrap());
    let reconcile = store.mission_run_for_reconcile(id).unwrap();
    assert_eq!(serde_json::to_value(&reconcile).unwrap(), serde_json::to_value(&expected).unwrap());
    if !actual.steps.is_empty() {
        let mission_id = actual.mission.strip_prefix("mission/").unwrap_or(&actual.mission);
        let pinned = store.mission_spec(mission_id, Some(&actual.revision)).unwrap();
        assert_eq!(serde_json::to_value(mission).unwrap(), serde_json::to_value(pinned).unwrap());
    }
}

#[test]
fn evaluation_definition_reuse_preserves_work_history_and_extensions() {
    let snapshot = now_ms().saturating_add(10_000);
    let (store, id) = crate::reconcile::tests::mission_eval_reuse::fixture();
    assert_equivalent(&store, &id, snapshot);
    let view = store.mission_run_for_reconcile(&id).unwrap();
    assert_eq!(view.steps.len(), 20);
    for step in &view.steps {
        assert_eq!(step.queue.as_deref(), Some("findings"));
        assert_eq!(step.timeout_ms, Some(3_600_000));
        assert!(step.fresh_context);
        assert_eq!(step.timeout_extension_ms, 60_000);
    }
    // The generation remains pinned when a different current definition is published.
    publish_mission(&store, r#"version 2
mission "evaluation-history" state="ready" {
 goal "A newer definition must not replace a running generation's definition."
 step "replacement" { agentless }
}"#, "replacement-definition");
    assert_equivalent(&store, &id, snapshot);
}

#[test]
fn evaluation_definition_reuse_preserves_loops_and_descendant_views() {
    let snapshot = now_ms().saturating_add(10_000);
    let store = Store::open_memory("node").unwrap();
    publish_mission(&store, r#"version 2
mission "reuse-root" state="ready" {
 goal "Preserve loop and child run views."
 step "child" { agentless }
 loop "rounds" {
  max-rounds 2
  round { completion { when "all-steps-exhausted" } }
 }
}
mission "reuse-child" state="ready" {
 goal "Inspect descendant work."
 queue "child-work" {
  assigned-to "person/test"
  step "first" timeout="1h" { fresh-context }
  step "second" timeout="2h" { }
 }
}"#, "reuse-tree-definitions");
    let request = |mission: &str, key: &str| MissionRunRequest {
        mission: mission.into(), revision: None, workspace: "/tmp".into(),
        requester: Some("person/test".into()), mode: None, inputs: BTreeMap::new(), idempotency_key: key.into(),
    };
    let root = store.create_mission_run(&request("reuse-root", "reuse-root-run")).unwrap();
    let parent = &root.steps.iter().find(|step| step.step == "child").unwrap().subject;
    let child = store.create_child_mission_run(&request("reuse-child", "reuse-child-run"), &root, parent, None).unwrap();
    assert_equivalent(&store, &root.id, snapshot);
    assert_equivalent(&store, &child.id, snapshot);
    assert!(!store.mission_run_for_reconcile(&root.id).unwrap().loops.is_empty());
    store.set_step_state(&child.steps[0].subject, "working", None).unwrap();
    assert_equivalent(&store, &child.id, snapshot);
    {
        let _clock = Clock::at(snapshot);
        let run = store.mission_run_for_reconcile(&child.id).unwrap();
        let work = run.steps.iter().find(|step| step.subject == child.steps[0].subject).unwrap();
        assert!(work.execution_started_at_unix_ms.is_some());
        assert!(work.execution_elapsed_ms > 0);
    }
    // Expired held work must get the same effective state before definition enrichment.
    store.connection.write().execute(
        "UPDATE step_runs SET status='claimed', lease_owner='person/test', lease_incarnation='worker-one', lease_expires_at_unix_ms='0' WHERE subject=?1",
        [&child.steps[0].subject],
    ).unwrap();
    assert_equivalent(&store, &child.id, snapshot);
}

#[test]
fn evaluation_definition_reuse_preserves_missing_revision_and_empty_step_behavior() {
    let snapshot = now_ms().saturating_add(10_000);
    let (store, id) = crate::reconcile::tests::mission_eval_reuse::fixture();
    store.connection.write().execute("DELETE FROM mission_revisions", []).unwrap();
    assert_equivalent(&store, &id, snapshot);
    assert!(store.mission_run_for_evaluation(&id).unwrap().1.is_none());
    store.connection.write().execute("DELETE FROM step_runs", []).unwrap();
    assert_equivalent(&store, &id, snapshot);
    assert!(store.mission_run_for_evaluation(&id).unwrap().1.is_none());
}

#[test]
fn evaluation_definition_reuse_preserves_invalid_definition_errors() {
    let (store, id) = crate::reconcile::tests::mission_eval_reuse::fixture();
    store.connection.write().execute(
        "UPDATE mission_revisions SET body='not-json'", [],
    ).unwrap();
    let snapshot = now_ms();
    let legacy_error = {
        let connection = store.readers.get();
        legacy_run(&connection, &id, snapshot).unwrap_err()
    };
    let current_error = store.mission_run_for_evaluation(&id).unwrap_err();
    assert!(matches!(legacy_error, rusqlite::Error::FromSqlConversionFailure(..)));
    assert!(matches!(current_error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::FromSqlConversionFailure(..))));
    // With no steps, legacy reconciliation does not require parsing the definition.
    store.connection.write().execute("DELETE FROM step_runs", []).unwrap();
    let (view, mission) = store.mission_run_for_evaluation(&id).unwrap();
    assert!(view.steps.is_empty());
    assert!(mission.is_none());
}
