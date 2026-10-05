//! Native full task/plan observations; never reconstruct a list from tool deltas or transcripts.
//!
//! Claude PostToolUse is the successful tool boundary. TodoWrite replaces the entire checklist
//! (the native response's `newTodos`, or the successful input's `todos`). TaskCreate/TaskUpdate
//! responses are single-task deltas, not a complete list. Claude 2.1.287 selects their backing
//! list through explicit list overrides, teams, leader state, or the session, and can use a
//! native-state backend instead of files. Without an authenticated complete-list read surface
//! and effective list binding, those operations remain unobserved; guessing a session task
//! directory would publish somebody else's list or a partial/empty snapshot as authoritative.
//!
//! Codex `turn/plan/updated` carries the full plan in its typed `plan` array. Plans remain
//! separate from todos, and neither an absent event nor a malformed source means known empty.
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};
use st3_schema::{
    HARNESS_TODO_MAX_FIELDS_BYTES, HARNESS_TODO_MAX_TASKS, HARNESS_TODO_MAX_TEXT_BYTES,
};

/// Observe only top-level successful TodoWrite calls belonging to the native session bound by
/// SessionStart. The outbox's transactional owner fence also rejects superseded runtimes.
pub fn observe_claude(agent_dir: &Path, event: &str, payload: &Value, runtime: &str) -> Result<()> {
    if !crate::harness_events::enabled(agent_dir)
        || event != "PostToolUse"
        || payload["tool_name"].as_str() != Some("TodoWrite")
        || payload["agent_id"].as_str().is_some_and(|id| !id.is_empty())
        || runtime.is_empty()
    {
        return Ok(());
    }
    let Some(session) = payload["session_id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    // Prefer the native successful response. A present malformed response must not be replaced
    // with plausible input; absence alone permits the documented full replacement input.
    let source = if payload.get("tool_response").is_some() {
        payload.pointer("/tool_response/newTodos")
    } else {
        payload.pointer("/tool_input/todos")
    };
    let Some(source) = source else {
        return Ok(());
    };
    let body = normalize(source, "claude", session, runtime, "TodoWrite", false)?;
    crate::harness_events::write_snapshot(agent_dir, "harness-todo", &body)
}

/// The control pump supplies its authenticated thread binding after subscription. Ignore all
/// other threads (including subagents) and all incremental item/delta messages.
pub fn observe_codex(
    agent_dir: &Path,
    message: &Value,
    bound_thread: &str,
    runtime: &str,
) -> Result<()> {
    if !crate::harness_events::enabled(agent_dir)
        || message["method"].as_str() != Some("turn/plan/updated")
        || bound_thread.is_empty()
        || runtime.is_empty()
        || message.pointer("/params/threadId").and_then(Value::as_str) != Some(bound_thread)
        || message.pointer("/params/turnId").and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Ok(());
    }
    let Some(source) = message.pointer("/params/plan") else {
        return Ok(());
    };
    let body = normalize(source, "codex", bound_thread, runtime, "turn/plan/updated", true)?;
    crate::harness_events::write_snapshot(agent_dir, "harness-plan", &body)
}

fn normalize(
    source: &Value,
    harness: &str,
    session: &str,
    runtime: &str,
    source_op: &str,
    plan: bool,
) -> Result<Vec<u8>> {
    let entries = source.as_array().context("native task snapshot is not a complete array")?;
    let mut tasks = Vec::with_capacity(entries.len().min(HARNESS_TODO_MAX_TASKS));
    let mut task_bytes = [0; HARNESS_TODO_MAX_TASKS];
    let mut retained_bytes = 0;
    let mut totals = [0_u64; 3];
    let mut truncated = false;
    for entry in entries {
        let content = entry[if plan { "step" } else { "content" }]
            .as_str().context("native task snapshot has no task content")?;
        let (status, index) = match entry["status"].as_str() {
            Some("pending") => ("pending", 0),
            Some("inProgress") if plan => ("in_progress", 1),
            Some("in_progress") if !plan => ("in_progress", 1),
            Some("completed") => ("completed", 2),
            _ => anyhow::bail!("native task snapshot has an unsupported status"),
        };
        totals[index] += 1;
        if tasks.len() == HARNESS_TODO_MAX_TASKS {
            truncated = true;
            continue;
        }
        let mut end = content.len().min(HARNESS_TODO_MAX_TEXT_BYTES);
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        truncated |= end != content.len();
        let task = json!({"content": &content[..end], "status": status});
        let bytes = serialized_size(&task)?;
        task_bytes[tasks.len()] = bytes;
        retained_bytes += bytes;
        tasks.push(task);
    }
    let mut value = json!({
        "incarnation": runtime,
        "harness": harness,
        "session_id": session,
        "incarnation_id": runtime,
        "observed_at": crate::exec_backend::rfc3339_utc(SystemTime::now())?,
        "source_op": source_op,
        "phases": [],
        "totals": {
            "pending": totals[0], "in_progress": totals[1], "completed": totals[2],
            "blocked": 0, "abandoned": 0,
        },
        "truncated": false,
    });
    if plan {
        value["version"] = 1.into();
    }
    // Count escaped JSON before allocating wire bytes. The fixed empty phase wrapper replaces
    // `[]`; row separators add one byte each. `false` is the conservative (longer) flag value.
    let base_bytes = serialized_size(&value)?;
    anyhow::ensure!(
        base_bytes <= HARNESS_TODO_MAX_FIELDS_BYTES,
        "native task snapshot provenance exceeds the wire size bound"
    );
    let phase_bytes = br#"[{"name":"","tasks":[]}]"#.len() - 2;
    while !tasks.is_empty()
        && base_bytes + phase_bytes + retained_bytes + tasks.len() - 1
            > HARNESS_TODO_MAX_FIELDS_BYTES
    {
        tasks.pop();
        retained_bytes -= task_bytes[tasks.len()];
        truncated = true;
    }
    value["truncated"] = truncated.into();
    if !tasks.is_empty() {
        // Both native sources are a flat list. A single unnamed phase preserves that fact.
        value["phases"] = json!([{"name": "", "tasks": tasks}]);
    }
    Ok(serde_json::to_vec(&value)?)
}

fn serialized_size(value: &Value) -> Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_agent() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        crate::harness_events::enable(root.path(), "seat-runtime").unwrap();
        crate::harness_events::write_snapshot(root.path(), "harness-state",
            br#"{"incarnation":"provider-runtime"}"#).unwrap();
        crate::harness_events::bind_claude_session(
            root.path(), "provider-runtime", "native-session").unwrap();
        root
    }

    fn claude(todos: Value) -> Value {
        json!({"session_id":"native-session", "tool_name":"TodoWrite",
            "tool_input":{"todos":todos}})
    }

    #[test]
    fn subagent_and_wrong_session_cannot_replace_parent_checklist() {
        let root = enabled_agent();
        let original = claude(json!([{"content":"Parent work", "status":"in_progress"}]));
        observe_claude(root.path(), "PostToolUse", &original, "provider-runtime").unwrap();
        let mut other = claude(json!([]));
        other["agent_id"] = "child".into();
        observe_claude(root.path(), "PostToolUse", &other, "provider-runtime").unwrap();
        other.as_object_mut().unwrap().remove("agent_id");
        other["session_id"] = "different-session".into();
        observe_claude(root.path(), "PostToolUse", &other, "provider-runtime").unwrap_err();
        let stored: Value = serde_json::from_slice(&crate::harness_events::read_snapshot(
            root.path(), "harness-todo").unwrap().unwrap()).unwrap();
        assert_eq!(stored["phases"][0]["tasks"][0]["content"], "Parent work");
        assert_eq!(stored["totals"]["in_progress"], 1);
    }

    #[test]
    fn clear_within_one_wrapper_refuses_a_prepared_predecessor_snapshot() {
        let root = enabled_agent();
        let old = normalize(&json!([]), "claude", "native-session", "provider-runtime",
            "TodoWrite", false).unwrap();
        crate::harness_events::bind_claude_session(
            root.path(), "provider-runtime", "cleared-session").unwrap();
        let mut current = claude(json!([{"content":"New session work","status":"pending"}]));
        current["session_id"] = "cleared-session".into();
        observe_claude(root.path(), "PostToolUse", &current, "provider-runtime").unwrap();
        let before = crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap();
        crate::harness_events::write_snapshot(root.path(), "harness-todo", &old).unwrap_err();
        assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap(), before);
        let stored: Value = serde_json::from_slice(before.as_ref().unwrap()).unwrap();
        assert_eq!(stored["session_id"], "cleared-session");
        assert_eq!(stored["phases"][0]["tasks"][0]["content"], "New session work");
    }

    #[test]
    fn unknown_deltas_and_failed_calls_do_not_clear_but_native_empty_does() {
        let root = enabled_agent();
        let original = claude(json!([{"content":"Work", "status":"pending"}]));
        observe_claude(root.path(), "PostToolUse", &original, "provider-runtime").unwrap();
        let before = crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap();
        for tool in ["TaskCreate", "TaskUpdate"] {
            let delta = json!({"session_id":"native-session", "tool_name":tool,
                "tool_response":{"task":{"subject":"Other", "status":"completed"}}});
            observe_claude(root.path(), "PostToolUse", &delta, "provider-runtime").unwrap();
        }
        observe_claude(root.path(), "PostToolUseFailure", &claude(json!([])), "provider-runtime").unwrap();
        observe_claude(root.path(), "PostToolUse", &claude(Value::Null), "provider-runtime").unwrap_err();
        assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap(), before);
        observe_claude(root.path(), "PostToolUse", &claude(json!([])), "provider-runtime").unwrap();
        let empty: Value = serde_json::from_slice(&crate::harness_events::read_snapshot(
            root.path(), "harness-todo").unwrap().unwrap()).unwrap();
        assert_eq!(empty["phases"], json!([]));
        assert_eq!(empty["totals"], json!({"pending":0,"in_progress":0,"completed":0,"blocked":0,"abandoned":0}));
        assert_eq!(empty["truncated"], false);
    }

    #[test]
    fn native_response_wins_and_malformed_full_source_is_not_a_prefix_snapshot() {
        let root = enabled_agent();
        let mut payload = claude(json!([{"content":"Input", "status":"pending"}]));
        payload["tool_response"] = json!({"newTodos":[{"content":"Actual", "status":"completed"}]});
        observe_claude(root.path(), "PostToolUse", &payload, "provider-runtime").unwrap();
        let before = crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap();
        let stored: Value = serde_json::from_slice(before.as_ref().unwrap()).unwrap();
        assert_eq!(stored["phases"][0]["tasks"][0], json!({"content":"Actual","status":"completed"}));
        for response in [json!({}), Value::Null, json!("incomplete")] {
            payload["tool_input"]["todos"] = json!([]);
            payload["tool_response"] = response;
            observe_claude(root.path(), "PostToolUse", &payload, "provider-runtime").unwrap();
            assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap(), before);
        }
        payload["tool_response"] = json!({"newTodos": []});
        let mut entries = vec![json!({"content":"Work", "status":"pending"}); 100];
        entries.push(json!({"content":"Invalid", "status":"unknown"}));
        observe_claude(root.path(), "PostToolUse", &claude(json!(entries)), "provider-runtime").unwrap_err();
        payload["tool_response"]["newTodos"] = Value::Null;
        observe_claude(root.path(), "PostToolUse", &payload, "provider-runtime").unwrap_err();
        assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap(), before);
    }

    #[test]
    fn bound_codex_plan_is_separate_and_owner_fenced() {
        let root = enabled_agent();
        observe_claude(root.path(), "PostToolUse", &claude(json!([])), "provider-runtime").unwrap();
        let todo = crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap();
        let message = json!({"method":"turn/plan/updated", "params":{
            "threadId":"thread", "turnId":"turn", "plan":[{"step":"Plan work","status":"inProgress"}]}});
        observe_codex(root.path(), &message, "other-thread", "provider-runtime").unwrap();
        assert!(crate::harness_events::read_snapshot(root.path(), "harness-plan").unwrap().is_none());
        observe_codex(root.path(), &message, "thread", "provider-runtime").unwrap();
        let plan: Value = serde_json::from_slice(&crate::harness_events::read_snapshot(
            root.path(), "harness-plan").unwrap().unwrap()).unwrap();
        assert_eq!(plan["version"], 1);
        assert_eq!(plan["phases"][0]["tasks"][0]["status"], "in_progress");
        assert_eq!(plan["session_id"], "thread");
        assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-todo").unwrap(), todo);
        let before = crate::harness_events::read_snapshot(root.path(), "harness-plan").unwrap();
        let mut empty = message.clone();
        empty["params"].as_object_mut().unwrap().remove("plan");
        observe_codex(root.path(), &empty, "thread", "provider-runtime").unwrap();
        assert_eq!(crate::harness_events::read_snapshot(root.path(), "harness-plan").unwrap(), before);
        empty["params"]["plan"] = json!([]);
        observe_codex(root.path(), &empty, "thread", "provider-runtime").unwrap();
        let cleared: Value = serde_json::from_slice(&crate::harness_events::read_snapshot(
            root.path(), "harness-plan").unwrap().unwrap()).unwrap();
        assert_eq!(cleared["phases"], json!([]));
        assert_eq!(cleared["totals"], json!({"pending":0,"in_progress":0,"completed":0,"blocked":0,"abandoned":0}));
        assert_eq!(cleared["truncated"], false);
        crate::harness_events::write_snapshot(root.path(), "harness-state", br#"{"incarnation":"successor"}"#).unwrap();
        observe_codex(root.path(), &message, "thread", "provider-runtime").unwrap_err();
        observe_claude(root.path(), "PostToolUse", &claude(json!([])), "provider-runtime").unwrap_err();
    }

    #[test]
    fn full_source_totals_survive_utf8_row_and_escaped_wire_bounds() {
        let entries = vec![json!({"content":"\u{0001}".repeat(512),"status":"pending"}); 101];
        let body = normalize(&json!(entries), "claude", "session", "runtime", "TodoWrite", false).unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.len() <= HARNESS_TODO_MAX_FIELDS_BYTES);
        assert!(value["phases"][0]["tasks"].as_array().unwrap().len() < 100);
        assert_eq!(value["totals"]["pending"], 101);
        assert_eq!(value["truncated"], true);
        let body = normalize(&json!([{"step":"€".repeat(200),"status":"completed"}]),
            "codex", "session", "runtime", "turn/plan/updated", true).unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["phases"][0]["tasks"][0]["content"], "€".repeat(170));
        assert_eq!(value["totals"]["completed"], 1);
        assert_eq!(value["truncated"], true);
    }
}
