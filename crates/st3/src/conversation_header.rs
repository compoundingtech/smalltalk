//! The conversation header (client contract `conversation-blocks.v1` §3, q3): the live summary
//! of a conversation, derived from the same normalized window a page was built from. Every field
//! names its source and the entry it came from, so a client can tell a derived summary from a
//! live one. Nothing here reads a transcript again or stores anything.
use serde_json::{Map, Value, json};

/// Terminal statuses for jobs and subagents: anything else is still in flight.
const TERMINAL_STATES: &[&str] = &[
    "completed",
    "failed",
    "cancelled",
    "canceled",
    "error",
    "done",
    "killed",
    "timeout",
    "timed_out",
    "exited",
    "success",
];

/// Derive the header fields the transcript window holds. Items may arrive in any order: "last"
/// is by `(timestamp, sequence)`, so both a chronological page and the paged cache agree. A
/// field is absent when the window holds nothing for it.
pub(crate) fn derive(items: &[Value], window_truncated: bool) -> Value {
    let mut order = (0..items.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| {
        (
            items[index]["timestamp"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            items[index]["sequence"].as_u64().unwrap_or(0),
        )
    });

    let mut model: Option<(Value, String)> = None;
    let mut context: Option<(u64, String)> = None;
    let mut cost: Option<(f64, String)> = None;
    let mut todo_output: Option<(Value, String)> = None;
    let mut todo_call: Option<(Value, String)> = None;
    let mut jobs: Vec<(String, Value)> = Vec::new();
    let mut jobs_at = String::new();
    let mut subagents: Vec<(String, Value)> = Vec::new();
    let mut subagents_at = String::new();
    let mut ask: Option<(String, Value, String)> = None;
    let mut answered: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for &index in &order {
        let item = &items[index];
        let at = item["timestamp"].as_str().unwrap_or_default().to_owned();
        let Some(blocks) = item["body"]["blocks"].as_array() else {
            continue;
        };
        for block in blocks {
            if let Some(metadata) = block
                .get("metadata")
                .filter(|metadata| metadata.is_object())
            {
                if let Some(name) = metadata.get("model").and_then(Value::as_str) {
                    model = Some((json!(name), at.clone()));
                }
                if let Some(tokens) = metadata.get("context_tokens").and_then(Value::as_u64) {
                    context = Some((tokens, at.clone()));
                }
                if let Some(usd) = metadata
                    .pointer("/usage/cost_usd")
                    .and_then(Value::as_f64)
                    .filter(|usd| usd.is_finite())
                {
                    let (sum, _) = cost.take().unwrap_or((0.0, String::new()));
                    cost = Some((sum + usd, at.clone()));
                }
            }
            let Some(view) = block.get("view").filter(|view| view.is_object()) else {
                continue;
            };
            let kind = block["kind"].as_str();
            match view.get("type").and_then(Value::as_str) {
                Some("todo") => match kind {
                    Some("tool_output") => {
                        if let Some(phases) = view.get("phases").filter(|phases| phases.is_array())
                        {
                            todo_output = Some((phases.clone(), at.clone()));
                        }
                    }
                    Some("tool_call") => {
                        let mut entry =
                            json!({"items": view.get("items").cloned().unwrap_or(json!([]))});
                        if let Some(phase) = view.get("phase").filter(|phase| !phase.is_null()) {
                            entry["phase"] = phase.clone();
                        }
                        todo_call = Some((json!([entry]), at.clone()));
                    }
                    _ => {}
                },
                Some("task") if kind == Some("tool_output") => {
                    for agent in view
                        .get("agents")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let Some(id) = agent.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        latest(
                            &mut subagents,
                            id.to_owned(),
                            agent.clone(),
                            terminal(agent.get("status").and_then(Value::as_str)),
                        );
                        subagents_at = at.clone();
                    }
                }
                Some("job" | "hub") => {
                    for job in view
                        .get("jobs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let Some(id) = job.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        latest(&mut jobs, id.to_owned(), job.clone(), job_terminal(job));
                        jobs_at = at.clone();
                    }
                }
                Some("ask") => match kind {
                    Some("tool_call") => {
                        ask = Some((
                            call_id(block, item).to_owned(),
                            view.get("questions").cloned().unwrap_or(json!([])),
                            at.clone(),
                        ));
                    }
                    Some("tool_output") => {
                        answered.insert(call_id(block, item).to_owned(), at.clone());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    let mut header = Map::new();
    if let Some((value, at)) = model {
        header.insert("model".into(), field(value, &at));
    }
    if let Some((tokens, at)) = context {
        header.insert(
            "context".into(),
            field(
                json!({"tokens": tokens, "window": window_truncated.then_some(true)}),
                &at,
            ),
        );
    }
    if let Some((usd, at)) = cost {
        let mut value = json!({"usd": usd});
        if window_truncated {
            value["window"] = json!(true);
        }
        header.insert("cost".into(), field(value, &at));
    }
    if let Some((value, at)) = todo_output.or(todo_call) {
        header.insert("todos".into(), field(value, &at));
    }
    if !jobs.is_empty() {
        let kept = jobs
            .into_iter()
            .filter(|(_, job)| !job_terminal(job))
            .map(|(_, job)| job)
            .collect::<Vec<_>>();
        header.insert("jobs".into(), field(json!(kept), &jobs_at));
    }
    if !subagents.is_empty() {
        let kept = subagents
            .into_iter()
            .map(|(_, agent)| agent)
            .collect::<Vec<_>>();
        header.insert("subagents".into(), field(json!(kept), &subagents_at));
    }
    if let Some((call_id, questions, at)) = ask {
        let (value, at) = match answered.get(&call_id) {
            Some(at) => (Value::Null, at.clone()),
            None => (json!({"call_id": call_id, "questions": questions}), at),
        };
        header.insert("ask".into(), field(value, &at));
    }
    Value::Object(header)
}

/// Overlay the live register: its fields win, with source `register`, over the derived ones.
/// Omit the header entirely when neither source supplies a field.
pub(crate) fn merge_register(mut derived: Value, register: Option<&Value>) -> Option<Value> {
    let target = derived.as_object_mut()?;
    if let Some(register) = register.and_then(Value::as_object) {
        for (name, entry) in register {
            target.insert(
                name.clone(),
                json!({
                    "value": entry.get("value").cloned().unwrap_or(Value::Null),
                    "source": "register",
                    "as_of": entry.get("as_of").cloned().unwrap_or(Value::Null),
                }),
            );
        }
    }
    (!target.is_empty()).then_some(derived)
}

/// The live latest-wins values for a conversation, e.g. `working`. Wire the register here when
/// draft #1583 (the live conversation register) lands; until then no register exists and the
/// header is derived from the transcript window alone.
pub(crate) fn register_value(_state: &crate::api::AppState, _session_id: &str) -> Option<Value> {
    None
}

/// Whether the window the items came from lost its prefix: the native fold's truncation entry,
/// or the bounded-query notices the projected timeline adds.
pub(crate) fn window_truncated(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item["type"] == "truncation"
            || matches!(
                item["body"]["code"].as_str(),
                Some("timeline-query-limited" | "timeline-history-incomplete")
            )
    })
}

fn field(value: Value, at: &str) -> Value {
    json!({"value": value, "source": "transcript", "as_of": at})
}

fn call_id<'a>(block: &'a Value, item: &'a Value) -> &'a str {
    block["payload"]["call_id"]
        .as_str()
        .or_else(|| item["body"]["call_id"].as_str())
        .unwrap_or_default()
}

fn terminal(status: Option<&str>) -> bool {
    status.is_some_and(|status| TERMINAL_STATES.contains(&status))
}

fn job_terminal(job: &Value) -> bool {
    job.get("ended_at").is_some_and(|value| !value.is_null())
        || job.get("exit_code").is_some_and(|value| !value.is_null())
        || terminal(job.get("state").and_then(Value::as_str))
}

/// Keep the latest entry per id: a terminal latest state drops the entry, any other replaces it.
fn latest(entries: &mut Vec<(String, Value)>, id: String, entry: Value, terminal: bool) {
    let Some(position) = entries.iter().position(|(known, _)| *known == id) else {
        if !terminal {
            entries.push((id, entry));
        }
        return;
    };
    if terminal {
        entries.remove(position);
    } else {
        entries[position].1 = entry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(sequence: u64, timestamp: &str, body: Value) -> Value {
        json!({
            "id": format!("timeline-entry/native-{sequence}"),
            "sequence": sequence,
            "revision": 1,
            "timestamp": timestamp,
            "role": "assistant",
            "type": "message",
            "final": true,
            "body": body,
        })
    }

    fn text_block(sequence: u64, metadata: Value) -> Value {
        json!({
            "id": format!("native-{sequence}/0"),
            "kind": "text",
            "source_type": "content",
            "payload": {"media_type": "text/plain", "text": "synthetic"},
            "metadata": metadata,
        })
    }

    fn tool_block(sequence: u64, kind: &str, body: Value, view: Value) -> Value {
        let source_type = if kind == "tool_call" {
            "tool_call"
        } else {
            "tool_result"
        };
        item(
            sequence,
            "2026-10-06T12:00:00Z",
            json!({
                "call_id": body.get("call_id").cloned().unwrap_or(json!(format!("call-{sequence}"))),
                "blocks": [{
                    "id": format!("native-{sequence}/0"),
                    "kind": kind,
                    "source_type": source_type,
                    "payload": body,
                    "view": view,
                }],
            }),
        )
    }

    #[test]
    fn every_field_derives_from_its_newest_entry() {
        let items = vec![
            // An older model that a later message replaces.
            item(
                1,
                "2026-10-06T12:00:01Z",
                json!({"blocks": [text_block(1, json!({
                    "model": "zai/glm-old",
                    "usage": {"cost_usd": 1.5},
                    "context_tokens": 1000,
                }))]}),
            ),
            // A todo call with no result yet: the call view stands in.
            tool_block(
                2,
                "tool_call",
                json!({"call_id": "call-todo", "arguments": {}}),
                json!({"type": "todo", "op": "write", "items": [
                    {"content": "first", "status": "pending"},
                ]}),
            ),
            // An ask that was answered, then a pending one.
            tool_block(
                3,
                "tool_call",
                json!({"call_id": "call-ask-1"}),
                json!({"type": "ask", "questions": [
                    {"id": "q1", "question": "which?", "options": [{"label": "a"}], "multi": false},
                ]}),
            ),
            tool_block(
                4,
                "tool_output",
                json!({"call_id": "call-ask-1", "content": {}}),
                json!({"type": "ask", "answers": [{"question": "which?", "selected": ["a"]}]}),
            ),
            tool_block(
                5,
                "tool_call",
                json!({"call_id": "call-ask-2"}),
                json!({"type": "ask", "questions": [
                    {"id": "q2", "question": "go?", "options": [{"label": "yes"}], "multi": false},
                ]}),
            ),
            // Jobs: one still running, one that finished, one replaced by a terminal state.
            item(
                6,
                "2026-10-06T12:00:06Z",
                json!({"blocks": [{
                    "id": "native-6/0", "kind": "job", "source_type": "custom_message",
                    "payload": {}, "view": {"type": "job", "jobs": [
                        {"id": "job-live", "name": "watch", "state": "running"},
                        {"id": "job-done", "name": "old", "state": "completed"},
                    ]},
                }]}),
            ),
            item(
                7,
                "2026-10-06T12:00:07Z",
                json!({"blocks": [{
                    "id": "native-7/0", "kind": "tool_output", "source_type": "tool_result",
                    "payload": {"call_id": "call-hub"},
                    "view": {"type": "hub", "op": "jobs", "jobs": [
                        {"id": "job-live", "name": "watch", "state": "running"},
                        {"id": "job-ended", "name": "build", "state": "running"},
                    ]},
                }]}),
            ),
            item(
                8,
                "2026-10-06T12:00:08Z",
                json!({"blocks": [{
                    "id": "native-8/0", "kind": "tool_output", "source_type": "tool_result",
                    "payload": {"call_id": "call-hub"},
                    "view": {"type": "hub", "op": "jobs", "jobs": [
                        {"id": "job-ended", "name": "build", "state": "completed", "exit_code": 0},
                    ]},
                }]}),
            ),
            // Subagents: one running, one that later completed.
            tool_block(
                9,
                "tool_output",
                json!({"call_id": "call-task", "content": {}}),
                json!({"type": "task", "async": true, "agents": [
                    {"id": "sub-live", "agent": "scout", "status": "running"},
                    {"id": "sub-over", "agent": "task", "status": "running"},
                ]}),
            ),
            tool_block(
                10,
                "tool_output",
                json!({"call_id": "call-task", "content": {}}),
                json!({"type": "task", "async": true, "agents": [
                    {"id": "sub-over", "agent": "task", "status": "completed"},
                ]}),
            ),
            // The newest assistant message: model, context and cost all move with it.
            item(
                11,
                "2026-10-06T12:00:11Z",
                json!({"blocks": [text_block(11, json!({
                    "model": "anthropic/claude-opus-5-5",
                    "usage": {"cost_usd": 0.25},
                    "context_tokens": 179053,
                }))]}),
            ),
        ];
        let header = derive(&items, false);
        assert_eq!(
            header["model"],
            json!({
                "value": "anthropic/claude-opus-5-5", "source": "transcript", "as_of": "2026-10-06T12:00:11Z",
            })
        );
        assert_eq!(
            header["context"]["value"],
            json!({"tokens": 179053, "window": null})
        );
        assert_eq!(header["context"]["as_of"], "2026-10-06T12:00:11Z");
        assert_eq!(header["cost"]["value"], json!({"usd": 1.75}));
        assert_eq!(header["cost"]["source"], "transcript");
        assert_eq!(header["cost"]["as_of"], "2026-10-06T12:00:11Z");
        assert_eq!(
            header["todos"]["value"],
            json!([{"items": [
                {"content": "first", "status": "pending"},
            ]}])
        );
        let jobs = header["jobs"]["value"].as_array().unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0]["id"], "job-live");
        let subagents = header["subagents"]["value"].as_array().unwrap();
        assert_eq!(subagents.len(), 1, "{subagents:?}");
        assert_eq!(subagents[0]["id"], "sub-live");
        assert_eq!(header["ask"]["value"]["call_id"], "call-ask-2");
        assert_eq!(header["ask"]["value"]["questions"][0]["id"], "q2");
        assert_eq!(header["ask"]["as_of"], "2026-10-06T12:00:00Z");
        // serde_json maps are key-sorted; only presence is contractual.
        assert_eq!(header.as_object().unwrap().len(), 7);

        // An answered ask settles to null at the answer's timestamp, and a truncated window
        // marks what it can only partially see.
        let answered = derive(&items[..4], true);
        assert_eq!(answered["ask"]["value"], Value::Null);
        assert_eq!(answered["ask"]["as_of"], "2026-10-06T12:00:00Z");
        assert_eq!(
            answered["context"]["value"],
            json!({"tokens": 1000, "window": true})
        );
        assert_eq!(answered["cost"]["value"]["window"], json!(true));
        assert!(answered.get("jobs").is_none());
        assert!(answered.get("subagents").is_none());
        // An empty window derives nothing at all.
        assert_eq!(derive(&[], false), json!({}));
    }

    #[test]
    fn order_independence_and_truncation_detection() {
        let chronological = vec![
            item(
                1,
                "2026-10-06T12:00:01Z",
                json!({"blocks": [text_block(1, json!({"model": "first/one"}))]}),
            ),
            item(
                2,
                "2026-10-06T12:00:02Z",
                json!({"blocks": [text_block(2, json!({"model": "second/two"}))]}),
            ),
        ];
        let mut newest_first = chronological.clone();
        newest_first.reverse();
        assert_eq!(
            derive(&chronological, false)["model"],
            derive(&newest_first, false)["model"]
        );
        assert_eq!(derive(&newest_first, false)["model"]["value"], "second/two");
        let mut truncated = newest_first.clone();
        truncated.insert(
            0,
            json!({"id": "timeline-entry/native-0", "sequence": 0, "revision": 1,
                "timestamp": "2026-10-06T11:59:00Z", "role": "system", "type": "truncation",
                "final": true, "body": {"reason": "bounded read window"}}),
        );
        assert!(window_truncated(&truncated));
        assert!(!window_truncated(&newest_first));
        let limited = vec![json!({
            "id": "x", "sequence": 1, "revision": 1, "timestamp": "2026-10-06T12:00:00Z",
            "role": "system", "type": "error", "final": true,
            "body": {"code": "timeline-query-limited"},
        })];
        assert!(window_truncated(&limited));
    }

    #[test]
    fn register_fields_win_over_derived_ones() {
        let derived = json!({
            "model": {"value": "derived/model", "source": "transcript", "as_of": "2026-10-06T12:00:01Z"},
            "cost": {"value": {"usd": 1.0}, "source": "transcript", "as_of": "2026-10-06T12:00:01Z"},
        });
        assert_eq!(merge_register(derived.clone(), None), Some(derived.clone()));
        let merged = merge_register(
            derived,
            Some(&json!({
                "model": {"value": "live/model", "as_of": "2026-10-06T12:00:09Z"},
                "working": {"value": true, "as_of": "2026-10-06T12:00:09Z"},
            })),
        )
        .expect("register supplies header fields");
        assert_eq!(
            merged["model"],
            json!({
                "value": "live/model", "source": "register", "as_of": "2026-10-06T12:00:09Z",
            })
        );
        assert_eq!(
            merged["working"],
            json!({
                "value": true, "source": "register", "as_of": "2026-10-06T12:00:09Z",
            })
        );
        assert_eq!(merged["cost"]["source"], "transcript");
    }

    #[test]
    fn absent_transcript_and_register_fields_omit_header() {
        let items = vec![item(
            1,
            "2026-10-06T12:00:00Z",
            json!({"text": "Plain Small Talk message"}),
        )];
        assert_eq!(merge_register(derive(&items, false), None), None);
        assert_eq!(
            merge_register(derive(&items, false), Some(&json!({}))),
            None,
        );
        let register = json!({
            "working": {"value": false, "as_of": "2026-10-06T12:00:09Z"},
        });
        assert_eq!(
            merge_register(derive(&items, false), Some(&register)),
            Some(json!({
                "working": {
                    "value": false,
                    "source": "register",
                    "as_of": "2026-10-06T12:00:09Z",
                },
            })),
        );
    }
}
