//! Mission evaluation reuse tests that build a reconciler fixture; they live in st3-daemon because the reconciler does.

#![allow(unused_imports)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::model::*;
use crate::store::Store;
use super::common::*;
use anyhow::Result;
use rusqlite::params;
use smallclaims::store::now_ms;
use st3_store::store::*;


struct Clock;

impl Clock {
fn at(at: u128) -> Self {
    smallclaims::store::set_thread_clock(Some(at));
    Self
}
}

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
    let timing = step_execution_clock_at(connection, &step.subject, step.attempt, snapshot,
        matches!(step.status.as_str(), "claimed" | "working"))?;
    step.execution_started_at_unix_ms = timing.started;
    step.execution_elapsed_ms = timing.elapsed;
    step.execution_paused_until_unix_ms = timing.paused_until;
    step.execution_waited_ms = timing.waited;
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

fn untimed_fixture() -> (Store, String) {
    let store = Store::open_memory("node").unwrap();
    publish_mission(&store, r#"version 2
mission "untimed-history" state="ready" {
 goal "Evaluate untimed history without replaying unused clocks."
 step "work" { agentless }
}"#, "untimed-history-definition");
    let run = store.create_mission_run(&MissionRunRequest {
        mission: "untimed-history".into(), revision: None, workspace: "/tmp".into(),
        requester: None, mode: None, inputs: BTreeMap::new(),
        idempotency_key: "untimed-history-run".into(),
    }).unwrap();
    (store, run.id)
}

// Insert history independently of projection order, as replicated canonical claims may arrive
// after newer claims. The projection's status/attempt is controlled separately by the test.
fn timing_claim(store: &Store, subject: &str, id: usize, kind: &str, at: u128, fields: Value) {
    store.connection.write().execute(
        "INSERT INTO claims(id,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms)
         VALUES(?1,(SELECT id FROM batches LIMIT 1),?2,?3,'node',NULL,?4,'[]',?5)",
        params![format!("timing-fixture-{id}"), subject, kind,
            json!({"fields": fields}).to_string(), at.to_string()],
    ).unwrap();
}

#[test]
fn evaluation_clock_elision_preserves_decisions_and_presentation() {
    const T: u128 = 1_800_000_000_000;
    let _clock = Clock::at(T);
    for timed in [false, true] {
        let (store, id) = if timed {
            let (store, id) = crate::reconcile::tests::mission_eval_reuse::fixture();
            (std::sync::Arc::try_unwrap(store).ok().unwrap(), id)
        } else {
            untimed_fixture()
        };
        let subject = store.mission_run_for_reconcile(&id).unwrap().steps[0].subject.clone();
        let history = [
            ("step-run.carried", 10, json!({"status":"working","claimant":"agent/node.worker","claim_expires_at_unix_ms":T+1000})),
            ("work.progress", 30, json!({"attempt":1,"clock":"paused","wait_until_unix_ms":T+100,"claim_expires_at_unix_ms":T+1000})),
            ("work.renewed", 40, json!({"attempt":1,"claim_expires_at_unix_ms":T+2000})),
            // Arrives late but sorts before the pause.
            ("work.claimed", 20, json!({"attempt":1,"claim_expires_at_unix_ms":T+1000})),
            ("work.progress", 70, json!({"attempt":1,"clock":"running","claim_expires_at_unix_ms":T+2000})),
            ("work.released", 80, json!({"attempt":1})),
            ("work.claimed", 90, json!({"attempt":2,"claim_expires_at_unix_ms":T+2000})),
            ("work.progress", 95, json!({"attempt":2,"clock":"paused","wait_until_unix_ms":T+150,"claim_expires_at_unix_ms":T+2000})),
        ];
        for (index, (kind, at, fields)) in history.into_iter().enumerate() {
            timing_claim(&store, &subject, index, kind, T + at, fields);
            for attempt in [1, 2] {
                for status in ["ready", "working", "completed"] {
                    store.connection.write().execute(
                        "UPDATE step_runs SET status=?1,attempt=?2 WHERE subject=?3",
                        params![status, attempt, subject],
                    ).unwrap();
                    for snapshot in [T + 50, T + 120, T + 2500] {
                        let _clock = Clock::at(snapshot);
                        let expected = legacy_run(&store.readers.get(), &id, snapshot).unwrap();
                        let presentation = store.mission_run(&id).unwrap().unwrap();
                        let (actual, mission) = store.mission_run_for_evaluation(&id).unwrap();
                        crate::reconcile::tests::mission_eval_reuse::assert_clock_decisions(
                            &actual, &expected, &mission.unwrap(),
                        );
                        let mut normalized = actual.clone();
                        for (actual, expected) in normalized.steps.iter_mut().zip(&expected.steps) {
                            if actual.timeout_ms.is_none() && !matches!(actual.status.as_str(), "claimed" | "working") {
                                actual.execution_started_at_unix_ms = expected.execution_started_at_unix_ms;
                                actual.execution_elapsed_ms = expected.execution_elapsed_ms;
                                actual.execution_waited_ms = expected.execution_waited_ms;
                                actual.timeout_extension_ms = expected.timeout_extension_ms;
                            }
                        }
                        assert_eq!(serde_json::to_value(normalized).unwrap(), serde_json::to_value(&expected).unwrap());
                        assert_eq!(serde_json::to_value(store.mission_run_for_reconcile(&id).unwrap()).unwrap(), serde_json::to_value(&expected).unwrap());
                        assert_eq!(serde_json::to_value(store.mission_run(&id).unwrap()).unwrap(), serde_json::to_value(&presentation).unwrap());
                        for step in &presentation.steps {
                            let timing = step_execution_clock_at(&store.readers.get(), &step.subject, step.attempt, snapshot,
                                matches!(step.status.as_str(), "claimed" | "working")).unwrap();
                            assert_eq!((step.execution_started_at_unix_ms, step.execution_elapsed_ms, step.execution_paused_until_unix_ms, step.execution_waited_ms),
                                (timing.started, timing.elapsed, timing.paused_until, timing.waited));
                        }
                    }
                }
            }
        }
    }
}
