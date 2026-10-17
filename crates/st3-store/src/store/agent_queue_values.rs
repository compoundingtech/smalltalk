//! The work-queue fields of an agent card: shared by the API's roster and the store's cached
//! agent rows, which compare them to decide whether a card changed.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::Store;
use crate::client_values::{client_timestamp, client_work_state};

pub const AGENT_QUEUE_FIELDS: [&str; 8] = [
    "current_work_ids", "active_work_count", "next_work_id", "upcoming_work_ids",
    "queued_work_count", "current_work", "next_work", "upcoming_work",
];

pub fn agent_queue_fields(
    queue: &crate::store::AgentWorkQueue,
    labels: &BTreeMap<String, crate::store::StepLabel>,
) -> Value {
    let label = |id: &String| labels.get(id).map(|step| json!({
        "id": id, "mission_id": step.mission, "mission_run_id": step.run,
        "path": step.path, "title": step.title, "goal": step.goal,
        "state": client_work_state(&step.status),
        "since": client_timestamp(step.updated_at_unix_ms),
    }));
    json!({
        "current_work_ids": queue.current_work_ids,
        "active_work_count": queue.active_work_count,
        "next_work_id": queue.next_work_id,
        "upcoming_work_ids": queue.upcoming_work_ids,
        "queued_work_count": queue.queued_work_count,
        "current_work": queue.current_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
        "next_work": queue.next_work_id.as_ref().and_then(label),
        "upcoming_work": queue.upcoming_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
    })
}

/// Refresh queue selection and presentation once, without folding any agent cards.
pub fn agent_queue_metadata(
    store: &Store,
    subjects: &BTreeSet<String>,
) -> anyhow::Result<Vec<Value>> {
    crate::otel::roster_phase("queue_selection", || -> anyhow::Result<Vec<Value>> {
        let queues = if subjects.len() == 1 {
            store.agent_work_queues_for(subjects.first().map(String::as_str))?
        } else {
            store.agent_work_queues()?
        };
        let steps = queues.values().flat_map(|queue| {
            queue.current_work_ids.iter().chain(queue.next_work_id.iter())
                .chain(queue.upcoming_work_ids.iter()).cloned()
        }).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let labels = store.step_labels(&steps)?;
        let empty = crate::store::AgentWorkQueue::default();
        Ok(subjects.iter().map(|subject| {
            let mut fields = agent_queue_fields(queues.get(subject).unwrap_or(&empty), &labels);
            fields["id"] = json!(subject);
            fields
        }).collect())
    })
}
