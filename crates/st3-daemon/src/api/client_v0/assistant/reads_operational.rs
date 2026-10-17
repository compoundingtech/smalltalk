//! Owner-authorized operational details. Native objects are never returned wholesale:
//! only the fields below cross this boundary, including at every nested level.
//! Text is limited to 512 UTF-8 bytes, nested lists to eight entries, definition steps
//! and dependencies to sixteen, and recent runs to three. The route bounds the full envelope.
use super::*;

const NESTED_ITEMS: usize = 8;
const DEFINITION_ITEMS: usize = 16;
const RECENT_RUNS: usize = 3;
const TEXT_BYTES: usize = 512;
const WORK_STATES: &[&str] = &[
    "waiting", "ready", "claimed", "verifying", "blocked", "completed", "failed",
    "cancelled", "skipped",
];

pub(super) async fn project_bound(
    state: &AppState,
    snapshot: &ClientSnapshot,
    projection: &str,
    parameters: &BTreeMap<String, String>,
    max_bytes: usize,
    binding: &str,
) -> Result<(ClientSnapshot, Value), ApiError> {
    // Also required for cursor continuations: a cached page cannot bypass owner admission.
    owner(state)?;
    match projection {
        "work" => page(state, snapshot, "assistant.work", parameters, max_bytes, work_rows, binding).await,
        "agent-queue" => queue_page(state, snapshot, parameters, max_bytes, binding).await,
        "mission-detail" | "work-detail" | "agent-detail" => {
            let (parameter, prefix) = match projection {
                "mission-detail" => ("mission_id", "mission/"),
                "work-detail" => ("work_id", "step-run/"),
                _ => ("agent_id", "agent/"),
            };
            allow(parameters, &[parameter, "max_response_bytes"], max_bytes)?;
            let id = selected_id(parameters, parameter, prefix)?.to_owned();
            let reader = state.clone();
            let projection = projection.to_owned();
            let (snapshot, value) = blocking_store(move || {
                reader.store.clone().read_snapshot(|index| {
                    let snapshot = client_snapshot_at(&reader, index);
                    let now = client_snapshot_time(&snapshot);
                    let value = match projection.as_str() {
                        "mission-detail" => {
                            let Some(source) = mission_resources(&reader.store, index, true, Some(&id))?
                                .into_iter().find(|row| row["id"].as_str() == Some(id.as_str()))
                            else { return Ok((snapshot, None)); };
                            let mut value = mission_detail(&source);
                            value["evidence_refs"] = evidence_refs(&reader.store, &id)?;
                            value
                        }
                        "work-detail" => {
                            let Some(source) = client_work_item(&reader.store, &id, None, now, index)?
                            else { return Ok((snapshot, None)); };
                            let mut value = work_detail(&source);
                            value["evidence_refs"] = evidence_refs(&reader.store, &id)?;
                            value
                        }
                        _ => {
                            let selected = BTreeSet::from([id.clone()]);
                            let mut sources = client_agent_resources_selected(
                                &reader.store, true, index, Some((&selected, &[])),
                            )?;
                            overlay_agent_resources(&reader.store, &mut sources, &client_timestamp(now))?;
                            let Some(source) = sources.iter().find(|row| row["id"].as_str() == Some(id.as_str()))
                            else { return Ok((snapshot, None)); };
                            agent_detail(source)
                        }
                    };
                    Ok((snapshot, Some(value)))
                })
            }).await?;
            Ok((snapshot, value.ok_or_else(|| ApiError::not_found("operational resource absent"))?))
        }
        _ => Err(ApiError::not_found("unknown assistant operational projection")),
    }
}

fn selected_id<'a>(
    parameters: &'a BTreeMap<String, String>, name: &str, prefix: &str,
) -> Result<&'a str, ApiError> {
    let id = required(parameters, name)?;
    if !id.starts_with(prefix) || id.len() == prefix.len() || ident(Some(id)).is_none() {
        return Err(validation(format!("`{name}` must be a {prefix} identifier")));
    }
    Ok(id)
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn text(value: &Value) -> Value {
    value.as_str().map_or(Value::Null, |source| {
        let mut end = source.len().min(TEXT_BYTES);
        while !source.is_char_boundary(end) { end -= 1; }
        json!(&source[..end])
    })
}

fn texts(value: &Value) -> Value {
    json!(items(value).iter().filter(|item| item.is_string()).take(NESTED_ITEMS).map(text).collect::<Vec<_>>())
}

fn number(value: &Value) -> Value {
    value.as_u64().map_or(Value::Null, |value| json!(value))
}

fn boolean(value: &Value) -> Value {
    value.as_bool().map_or(Value::Null, |value| json!(value))
}

/// One selected subject's newest native claim is a bounded provenance read. Expose only
/// its identifier and evidence identifiers, never claim fields, bodies or free-form payloads.
fn evidence_refs(store: &Store, subject: &str) -> anyhow::Result<Value> {
    let Some(claim) = store.latest_claim(subject, None)? else { return Ok(json!([])); };
    let mut refs = Vec::with_capacity(NESTED_ITEMS);
    if let Some(id) = ident(Some(&claim.id)) { refs.push(id); }
    for id in items(&claim.body["evidence"]).iter().take(NESTED_ITEMS - 1)
        .filter_map(|value| ident(value.as_str()))
    {
        // Evidence is untrusted input, not an arbitrary locator. Only a native
        // claim identifier that actually resolves may cross this boundary.
        if store.claim_by_id(&id)?.is_some() { refs.push(id); }
    }
    Ok(json!(refs))
}

fn work_detail(source: &Value) -> Value {
    json!({
        "id": ident(source["id"].as_str()),
        "mission_id": ident(source["mission_id"].as_str()),
        "mission_run_id": ident(source["mission_run_id"].as_str()),
        "generation_id": ident(source["generation_id"].as_str()),
        "definition_id": ident(source["definition_id"].as_str()),
        "revision": ident(source["revision"].as_str()),
        "title": text(&source["title"]),
        "state": closed(source["state"].as_str(), WORK_STATES),
        "assigned_to": ident(source["assigned_to"].as_str()),
        "claimant": ident(source["claimant"].as_str()),
        "agentless": boolean(&source["agentless"]),
        "attempt": number(&source["attempt"]),
        "readiness_epoch": number(&source["readiness_epoch"]),
        "lease": {
            "incarnation_id": ident(source["claim_incarnation"].as_str()),
            "expires_at_unix_ms": number(&source["claim_expires_at_unix_ms"]),
            "execution_started_at_unix_ms": number(&source["execution_started_at_unix_ms"]),
            "execution_elapsed_ms": number(&source["execution_elapsed_ms"]),
            "timeout_ms": number(&source["timeout_ms"]),
        },
        "goals": texts(&source["goals"]),
        "goals_truncated": items(&source["goals"]).len() > NESTED_ITEMS,
        "blocked_reason": text(&source["blocked_reason"]),
        "blockers": texts(&source["blockers"]),
        "blockers_truncated": items(&source["blockers"]).len() > NESTED_ITEMS,
        "readiness": {
            "actionable": boolean(&source["operational"]["actionable"]),
            "layer": closed(source["operational"]["layer"].as_str(), &["current", "history"]),
        },
        "updated_at": timestamp(&source["updated_at"]),
        "evidence_refs": [],
    })
}

fn work_rows(state: &AppState, _person: &str, index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    client_work_resources(&state.store, None, false, now, index)?.iter().map(|source| {
        let mut value = work_detail(source);
        if let Some(id) = source["id"].as_str() {
            value["evidence_refs"] = evidence_refs(&state.store, id)?;
        }
        Ok(value)
    }).collect()
}

fn step_detail(source: &Value) -> Value {
    json!({
        "id": ident(source["id"].as_str()), "title": text(&source["title"]),
        "state": closed(source["state"].as_str(), WORK_STATES),
        "assignee": ident(source["assignee"].as_str()),
        "claimant": ident(source["claimant"].as_str()),
        "attempt": number(&source["attempt"]), "agentless": boolean(&source["agentless"]),
        "goals": texts(&source["goals"]), "goals_truncated": items(&source["goals"]).len() > NESTED_ITEMS,
        "blocked_reason": text(&source["blocked_reason"]),
        "blockers": texts(&source["blockers"]), "blockers_truncated": items(&source["blockers"]).len() > NESTED_ITEMS,
        "since": timestamp(&source["since"]),
        "claim_expires_at": timestamp(&source["claim_expires_at"]),
    })
}

fn run_detail(source: &Value) -> Value {
    json!({
        "id": ident(source["id"].as_str()),
        "generation_id": ident(source["generation_id"].as_str()),
        "requester": ident(source["requester"].as_str()),
        "status": closed(source["status"].as_str(), MISSION_PHASES),
        "phase": closed(source["phase"].as_str(), &["pending", "active", "terminal", "standing"]),
        "state_since": timestamp(&source["state_since"]),
        "dependency_run_id": ident(source["after"].as_str()),
        "progress": {"done": number(&source["progress"]["done"]), "total": number(&source["progress"]["total"])},
        "blocker": {"step": ident(source["blocker"]["step"].as_str()), "reason": text(&source["blocker"]["reason"])},
        "steps": items(&source["steps"]).iter().take(NESTED_ITEMS).map(step_detail).collect::<Vec<_>>(),
        "steps_truncated": items(&source["steps"]).len() > NESTED_ITEMS,
    })
}

fn mission_detail(source: &Value) -> Value {
    let visualization = &source["visualization"];
    let runs = items(&source["run_details"]);
    json!({
        "id": ident(source["id"].as_str()), "revision": ident(source["revision"].as_str()),
        "title": text(&source["title"]), "state": closed(source["state"].as_str(), MISSION_PHASES),
        "updated_at": timestamp(&source["updated_at"]),
        "goals": texts(&visualization["goals"]), "goals_truncated": items(&visualization["goals"]).len() > NESTED_ITEMS,
        "steps": items(&visualization["nodes"]).iter().take(DEFINITION_ITEMS).map(|node| json!({
            "id": ident(node["id"].as_str()), "title": text(&node["label"]),
            "goals": texts(&node["goals"]), "goals_truncated": items(&node["goals"]).len() > NESTED_ITEMS,
            "evidence_refs": items(&node["source_references"]).iter().take(NESTED_ITEMS)
                .filter_map(|value| ident(value.as_str()).filter(|id| id.starts_with("document/"))).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "steps_truncated": items(&visualization["nodes"]).len() > DEFINITION_ITEMS,
        "dependencies": items(&visualization["edges"]).iter().take(DEFINITION_ITEMS).map(|edge| json!({
            "id": ident(edge["id"].as_str()), "from": ident(edge["from"].as_str()),
            "to": ident(edge["to"].as_str()), "kind": closed(edge["kind"].as_str(), &["dependency", "gate"]),
        })).collect::<Vec<_>>(),
        "dependencies_truncated": items(&visualization["edges"]).len() > DEFINITION_ITEMS,
        "recent_runs": runs.iter().rev().take(RECENT_RUNS).map(run_detail).collect::<Vec<_>>(),
        "runs_truncated": runs.len() > RECENT_RUNS,
        "active_runs": number(&source["active_runs"]),
        "evidence_refs": [],
    })
}

fn agent_detail(source: &Value) -> Value {
    json!({
        "id": ident(source["id"].as_str()), "revision": ident(source["revision"].as_str()),
        "state": closed(source["state"].as_str(), AGENT_STATES),
        "lifecycle": closed(source["lifecycle"].as_str(), AGENT_LIFECYCLES),
        "harness_state": closed(source["harness_state"].as_str(), &["working", "ready", "idle", "unauthenticated", "indeterminate"]),
        "reachability": closed(source["reachability"].as_str(), &["reachable", "unreachable", "indeterminate", "local", "remote"]),
        "owner_run_id": ident(source["owner_run_id"].as_str()),
        "incarnation_id": ident(source["incarnation_id"].as_str()),
        "updated_at": timestamp(&source["updated_at"]), "since": timestamp(&source["since"]),
        "last_activity_at": timestamp(&source["last_activity_at"]),
        "current_work": items(&source["current_work"]).iter().take(NESTED_ITEMS).map(|work| json!({
            "id": ident(work["id"].as_str()), "mission_id": ident(work["mission_id"].as_str()),
            "mission_run_id": ident(work["mission_run_id"].as_str()),
            "state": closed(work["state"].as_str(), WORK_STATES),
        })).collect::<Vec<_>>(),
        "current_work_truncated": items(&source["current_work"]).len() > NESTED_ITEMS,
        "queued_work_count": number(&source["queued_work_count"]),
    })
}

async fn queue_page(
    state: &AppState, snapshot: &ClientSnapshot, parameters: &BTreeMap<String, String>, max_bytes: usize,
    binding: &str,
) -> Result<(ClientSnapshot, Value), ApiError> {
    allow(parameters, &["agent_id", "cursor", "limit", "max_response_bytes"], max_bytes)?;
    let id = selected_id(parameters, "agent_id", "agent/")?.to_owned();
    let limit = parameters.get("limit").map(|value| value.parse::<usize>().ok()
        .filter(|limit| (1..=CLIENT_MAX_PAGE_ITEMS).contains(limit))
        .ok_or_else(|| validation(format!("limit must be between 1 and {CLIENT_MAX_PAGE_ITEMS}"))))
        .transpose()?;
    let query = ClientListQuery { limit, cursor: parameters.get("cursor").cloned(), ..Default::default() };
    // The collection participates in cursor validation and the cache key, so another
    // agent's otherwise valid cursor cannot address this agent's queue.
    let collection = format!("assistant.agent-queue:{id}:{binding}");
    if query.cursor.is_some() {
        let result = client_page(state, snapshot, &collection, Vec::new(), &query)?;
        return Ok((snapshot.clone(), serde_json::to_value(result).map_err(ApiError::internal)?));
    }
    let reader = state.clone();
    let (snapshot, rows) = blocking_store(move || reader.store.clone().read_snapshot(|index| {
        let snapshot = client_snapshot_at(&reader, index);
        if reader.store.latest_claim(&id, None)?.is_none() {
            return Ok((snapshot, None));
        }
        let queue = reader.store.seat_queue(&id)?;
        let rows = queue.runs.iter().map(|run| json!({
            "agent_id": ident(Some(&queue.agent)), "mission_run_id": ident(Some(&run.run)),
            "position": run.position,
            "state": closed(Some(&run.state), &["claimed", "ready", "waiting"]),
            "run_status": closed(Some(&run.run_status), MISSION_PHASES),
            "joined_at": client_timestamp(run.joined_at_unix_ms),
            "claimed_work_ids": run.claimed_work_ids.iter().take(NESTED_ITEMS).filter_map(|id| ident(Some(id))).collect::<Vec<_>>(),
            "ready_work_ids": run.ready_work_ids.iter().take(NESTED_ITEMS).filter_map(|id| ident(Some(id))).collect::<Vec<_>>(),
            "waiting_work_ids": run.waiting_work_ids.iter().take(NESTED_ITEMS).filter_map(|id| ident(Some(id))).collect::<Vec<_>>(),
            "work_ids_truncated": ([run.claimed_work_ids.len(), run.ready_work_ids.len(), run.waiting_work_ids.len()].into_iter().any(|len| len > NESTED_ITEMS)),
            "waiting_for": ident(run.waiting_for.as_deref()),
        })).collect::<Vec<_>>();
        Ok((snapshot, Some(rows)))
    })).await?;
    let rows = rows.ok_or_else(|| ApiError::not_found("agent absent"))?;
    let result = client_page_read(state, &snapshot, &collection, rows, &query, true)?;
    Ok((snapshot, serde_json::to_value(result).map_err(ApiError::internal)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_details_drop_unlisted_nested_fields() {
        let source = json!({"id":"step-run/run/build", "state":"ready", "environment":{"TOKEN":"hidden"},
            "workspace":"/private/work", "lease":{"secret":"hidden"}, "operational":{"actionable":true,"secret":"hidden"},
            "person_answers":[{"answer":"hidden"}], "goals":["Build"], "blockers":["dependency"]});
        let projected = work_detail(&source);
        assert_eq!(projected["goals"], json!(["Build"]));
        assert_eq!(projected["readiness"]["actionable"], true);
        assert!(!serde_json::to_string(&projected).unwrap().contains("hidden"));
        assert!(projected.get("workspace").is_none());
        let agent = agent_detail(&json!({"id":"agent/test", "environment":{"token":"hidden"},
            "current_work":[{"id":"step-run/r/s", "terminal":"hidden", "workspace":"hidden"}]}));
        assert!(!serde_json::to_string(&agent).unwrap().contains("hidden"));
    }

    #[test]
    fn mission_nested_fields_and_runs_are_bounded() {
        let source = json!({"id":"mission/test", "visualization":{
            "goals": vec!["x".repeat(1024); 20],
            "nodes": vec![json!({"id":"step/a","label":"title","assignment":{"environment":"hidden"},
                "source_references":["document/spec", "/private/workspace"],"goals":["goal"]}); 20],
            "edges": vec![json!({"from":"step/a","to":"step/b","gate":{"secret":"hidden"},"kind":"dependency"}); 20]},
            "run_details": (0..10).map(|i| json!({"id":format!("mission-run/{i}"),"steps":vec![json!({"id":"step-run/r/s"}); 20]})).collect::<Vec<_>>()});
        let result = mission_detail(&source);
        assert_eq!(items(&result["steps"]).len(), DEFINITION_ITEMS);
        assert_eq!(items(&result["dependencies"]).len(), DEFINITION_ITEMS);
        assert_eq!(items(&result["recent_runs"]).len(), RECENT_RUNS);
        assert_eq!(result["recent_runs"][0]["id"], "mission-run/9");
        assert_eq!(result["steps"][0]["evidence_refs"], json!(["document/spec"]));
        assert_eq!(result["steps_truncated"], true);
        assert!(!serde_json::to_string(&result).unwrap().contains("hidden"));
    }

    #[test]
    fn text_caps_preserve_utf8_and_identifiers_are_validated() {
        let value = text(&json!("界".repeat(300)));
        assert!(value.as_str().unwrap().len() <= TEXT_BYTES);
        assert_eq!(items(&texts(&json!(vec!["goal"; 20]))).len(), NESTED_ITEMS);
        let parameters = BTreeMap::from([("agent_id".into(), "/private/work".into())]);
        assert!(selected_id(&parameters, "agent_id", "agent/").is_err());
    }
}
