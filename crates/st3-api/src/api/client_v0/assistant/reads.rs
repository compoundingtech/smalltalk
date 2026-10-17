//! Minimal assistant read projections. Each one reuses a native snapshot reader and copies
//! only allowlisted identifiers, closed enums, counts and timestamps out of it, so titles,
//! goals, previews, paths and other free text in source rows never reach an assistant.
//! Content reads return one bounded text range of one exactly named resource.
use super::*;

/// Default and ceiling of an assistant response, including its whole JSON envelope. The
/// route enforces the envelope; content sources read no more than this many bytes.
pub const DEFAULT_RESPONSE_BYTES: usize = 4 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;

const ATTENTION_KINDS: &[&str] = &[
    "human-gate", "launch-approval", "revision-approval", "unread-message", "person-step",
    "agent-request", "fault", "harness-login", "harness-prompt",
];
const AGENT_STATES: &[&str] = &[
    "desired", "starting", "running", "waiting", "stopped", "failed", "suspended",
];
const AGENT_LIFECYCLES: &[&str] = &["standing", "owner", "bounded"];
const MISSION_PHASES: &[&str] = &[
    "draft", "ready", "running", "standing", "blocked", "completed", "failed", "cancelled",
    "retired",
];

const METADATA_FIELDS: &[&str] = &["cursor", "limit", "max_response_bytes"];
const SUMMARY_FIELDS: &[&str] = &["max_response_bytes"];
const CONVERSATION_FIELDS: &[&str] = &[
    "agent_id", "session_id", "reference", "revision", "start", "end", "max_response_bytes",
];
const TERMINAL_FIELDS: &[&str] = &[
    "agent_id", "terminal_id", "incarnation_id", "screen_revision", "max_response_bytes",
];

/// Project one assistant read with the snapshot its rows were read in. `parameters` are the
/// exact decoded query parameters the read authorization names; the person is the
/// configured owner, never a parameter. A cursor or content read keeps `snapshot`.
pub async fn project(
    state: &AppState,
    snapshot: &ClientSnapshot,
    projection: &str,
    parameters: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<(ClientSnapshot, Value), ApiError> {
    match projection {
        "summary" => {
            allow(parameters, SUMMARY_FIELDS, max_bytes)?;
            summary(state).await
        }
        "attention" => page(state, snapshot, "assistant.attention", parameters, max_bytes, attention_rows).await,
        "attention-rich" => page(state, snapshot, "assistant.attention-rich", parameters, max_bytes, attention_rich_rows).await,
        "missions-rich" => page(state, snapshot, "assistant.missions-rich", parameters, max_bytes, mission_rich_rows).await,
        "agents-rich" => page(state, snapshot, "assistant.agents-rich", parameters, max_bytes, agent_rich_rows).await,
        "attention-content" => {
            allow(parameters, &["alert_id","max_response_bytes"], max_bytes)?;
            let id = required(parameters, "alert_id")?;
            let item = state.store.attention_items(Some(&owner(state)?)).map_err(ApiError::internal)?
                .into_iter().find(|item| item.subject == id || client_attention_id(&item.subject,&item.person,&item.episode).ok().as_deref() == Some(id))
                .ok_or_else(|| ApiError::not_found("owner alert absent"))?;
            Ok((snapshot.clone(), json!({"id":item.subject,"episode":item.episode,"kind":item.kind,
                "title":item.title,"detail":item.detail,"request":item.request})))
        },
        "missions" => page(state, snapshot, "assistant.missions", parameters, max_bytes, mission_rows).await,
        "agents" => page(state, snapshot, "assistant.agents", parameters, max_bytes, agent_rows).await,
        "conversation-content" => Ok((snapshot.clone(), conversation(state, parameters, max_bytes).await?)),
        "terminal-content" => Ok((snapshot.clone(), terminal(state, parameters, max_bytes).await?)),
        _ => Err(ApiError::not_found("unknown assistant projection")),
    }
}

/// Every parameter is named by the projection; a stated budget must be the one enforced.
fn allow(
    parameters: &BTreeMap<String, String>,
    fields: &[&str],
    max_bytes: usize,
) -> Result<(), ApiError> {
    if !(1..=MAX_RESPONSE_BYTES).contains(&max_bytes) {
        return Err(validation(format!("max_response_bytes must be between 1 and {MAX_RESPONSE_BYTES}")));
    }
    if let Some(unknown) = parameters.keys().find(|key| !fields.contains(&key.as_str())) {
        return Err(validation(format!("query parameter `{unknown}` is not allowed here")));
    }
    if let Some(stated) = parameters.get("max_response_bytes")
        && stated.parse::<usize>().ok() != Some(max_bytes)
    {
        return Err(validation("max_response_bytes does not match the enforced budget"));
    }
    Ok(())
}

fn required<'a>(parameters: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, ApiError> {
    parameters
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| validation(format!("query parameter `{name}` is required")))
}

fn budget_exceeded(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        code: "response-budget-exceeded".into(),
        message: message.into(),
        details: Box::default(),
    }
}

/// An identifier copied out of a source row, or nothing. Identifiers are short tokens; a value
/// with spaces, controls or prose punctuation is source text, which never leaves.
fn ident(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            (1..=256).contains(&value.len())
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || b"/._:-@+=~".contains(&byte)
                })
        })
        .map(str::to_owned)
}

fn closed(value: Option<&str>, allowed: &[&'static str]) -> &'static str {
    value
        .and_then(|value| allowed.iter().find(|item| **item == value))
        .copied()
        .unwrap_or("unknown")
}

fn attention_kind(kind: &str) -> &'static str {
    if kind.starts_with("custom.") {
        "custom"
    } else {
        closed(Some(kind), ATTENTION_KINDS)
    }
}

/// A timestamp the native reader rendered, kept only if it is one.
fn timestamp(value: &Value) -> Value {
    value
        .as_str()
        .filter(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok())
        .map_or(Value::Null, |at| json!(at))
}

fn owner(state: &AppState) -> Result<String, ApiError> {
    state
        .owning_person
        .clone()
        .filter(|person| ident(Some(person.as_str())).is_some() && person.starts_with("person/"))
        .ok_or_else(|| forbidden("assistant reads require a configured owning person"))
}

type Rows = fn(&AppState, &str, u64, u128) -> anyhow::Result<Vec<Value>>;

/// One page in the existing snapshot/page envelope. A cursor continues its first page's
/// cached rows and re-reads nothing; the page never grows past one limit to fit a budget.
/// The collection names the exact projector/content class: both the cursor and its cached
/// rows bind it, so a rich cursor cannot be continued by a metadata-only authorization.
async fn page(
    state: &AppState,
    snapshot: &ClientSnapshot,
    collection: &'static str,
    parameters: &BTreeMap<String, String>,
    max_bytes: usize,
    rows: Rows,
) -> Result<(ClientSnapshot, Value), ApiError> {
    allow(parameters, METADATA_FIELDS, max_bytes)?;
    let limit = parameters
        .get("limit")
        .map(|limit| {
            limit
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=CLIENT_MAX_PAGE_ITEMS).contains(limit))
                .ok_or_else(|| validation(format!("limit must be between 1 and {CLIENT_MAX_PAGE_ITEMS}")))
        })
        .transpose()?;
    let query = ClientListQuery {
        limit,
        cursor: parameters.get("cursor").cloned(),
        ..Default::default()
    };
    let person = owner(state)?;
    // As `client_snapshot_page`: a cursor continues the cached first page of the snapshot it
    // was issued in; a first page is read inside one SQLite snapshot and names that snapshot.
    if query.cursor.is_some() {
        let page = client_page(state, snapshot, collection, Vec::new(), &query)?;
        return Ok((snapshot.clone(), serde_json::to_value(page).map_err(ApiError::internal)?));
    }
    let reader = state.clone();
    let (snapshot, items) = blocking_store(move || {
        reader.store.clone().read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let now = client_snapshot_time(&snapshot);
            let items = reader
                .store
                .with_owned_set_snapshot_reads(|| rows(&reader, &person, index, now))?;
            Ok((snapshot, items))
        })
    })
    .await?;
    let page = client_page_read(state, &snapshot, collection, items, &query, true)?;
    Ok((snapshot, serde_json::to_value(page).map_err(ApiError::internal)?))
}

async fn summary(state: &AppState) -> Result<(ClientSnapshot, Value), ApiError> {
    let person = owner(state)?;
    let reader = state.clone();
    blocking_store(move || {
        reader.store.clone().read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let now = client_snapshot_time(&snapshot);
            let value = reader.store.with_owned_set_snapshot_reads(|| {
                let attention = attention_rows(&reader, &person, index, now)?.len();
                let count = |rows: Vec<Value>, field: &str| {
                    let mut counts = BTreeMap::<String, u64>::new();
                    for row in rows {
                        if let Some(value) = row[field].as_str() {
                            *counts.entry(value.to_owned()).or_default() += 1;
                        }
                    }
                    counts
                };
                let agents = count(agent_rows(&reader, &person, index, now)?, "runtime_state");
                let missions = count(mission_rows(&reader, &person, index, now)?, "phase");
                anyhow::Ok(json!({
                    "kind": "assistant-summary",
                    "as_of": client_timestamp(now),
                    "counts": {
                        "attention": attention,
                        "agent_states": agents,
                        "mission_states": missions,
                    },
                }))
            })?;
            Ok((snapshot, value))
        })
    })
    .await
}

/// The owner's open attention: identifiers, kind, episode and when it began waiting.
fn attention_rows(state: &AppState, person: &str, _index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    let store = &state.store;
    let mut rows = Vec::new();
    for item in store.attention_snapshot(Some(person), now)? {
        if item.person != person {
            continue;
        }
        // An episode that is not identifier-shaped is source text: the row stays, unnamed.
        let episode = ident(Some(item.episode.as_str())).unwrap_or_default();
        let mission = match &item.mission {
            Some(mission) => Some(mission.clone()),
            None if item.kind == "person-step" => store
                .person_ask_context(&item.subject)?
                .and_then(|context| context["mission_id"].as_str().map(str::to_owned)),
            None => None,
        };
        let agent = [item.conversation.as_deref(), item.requester_id.as_deref()]
            .into_iter()
            .flatten()
            .find(|agent| agent.starts_with("agent/"));
        rows.push(json!({
            "id": client_attention_id(&item.subject, &item.person, &item.episode)?,
            "attention_kind": attention_kind(&item.kind),
            "person_id": person,
            "agent_id": ident(agent),
            "mission_id": ident(mission.as_deref()),
            "mission_run_id": ident(item.mission_run.as_deref()),
            "revision": episode,
            "episode": episode,
            "waiting_since": client_timestamp(item.requested_at_unix_ms),
        }));
    }
    Ok(rows)
}

fn attention_rich_rows(state: &AppState, person: &str, index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    let mut rows = attention_rows(state,person,index,now)?;
    let items = state.store.attention_snapshot(Some(person),now)?;
    for row in &mut rows {
        for item in &items {
            if row["episode"] == item.episode {
                row["title"] = json!(item.title);
                row["detail"] = json!(item.detail);
                row["request"] = json!(item.request);
                row["target_id"] = json!(item.subject);
                row["evidence"] = json!(item.targets);
            }
        }
    }
    Ok(rows)
}
fn mission_rich_rows(state: &AppState, person: &str, index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    let mut rows = mission_rows(state,person,index,now)?;
    let source = mission_resources(&state.store,index,false,None)?;
    for row in &mut rows {
        if let Some(mission) = source.iter().find(|mission| mission["id"] == row["id"]) {
            row["title"] = mission["title"].clone();
        }
    }
    Ok(rows)
}
fn agent_rich_rows(state: &AppState, person: &str, index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    let mut rows = agent_rows(state,person,index,now)?;
    for row in &mut rows { row["title"] = json!(row["id"].as_str().unwrap_or("").trim_start_matches("agent/")); }
    Ok(rows)
}

/// Current missions: phase, the open (else newest) run, its step and blocked counts and the
/// agents it names. Goals, titles, step paths and status prose stay in the source.
fn mission_rows(state: &AppState, _person: &str, index: u64, _now: u128) -> anyhow::Result<Vec<Value>> {
    let mut rows = Vec::new();
    for mission in mission_resources(&state.store, index, false, None)? {
        let Some(id) = ident(mission["id"].as_str()) else {
            continue;
        };
        let runs = mission["run_details"].as_array().map(Vec::as_slice).unwrap_or_default();
        let open = |run: &&Value| !matches!(run["status"].as_str(), Some("completed" | "failed" | "cancelled"));
        let run = runs.iter().rev().find(open).or_else(|| runs.last());
        let (run_id, total, completed, blocked, agents) = match run {
            Some(run) => {
                let steps = run["steps"]
                    .as_array()
                    .or_else(|| run["current_steps"].as_array())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let agents = steps
                    .iter()
                    .flat_map(|step| [step["assignee"].as_str(), step["claimant"].as_str()])
                    .flatten()
                    .filter(|agent| agent.starts_with("agent/"))
                    .filter_map(|agent| ident(Some(agent)))
                    .collect::<BTreeSet<_>>();
                (
                    ident(run["id"].as_str()),
                    run["progress"]["total"].as_u64().unwrap_or(0),
                    run["progress"]["done"].as_u64().unwrap_or(0),
                    steps.iter().filter(|step| step["state"] == "blocked").count(),
                    agents,
                )
            }
            None => (None, 0, 0, 0, BTreeSet::new()),
        };
        rows.push(json!({
            "id": id,
            "run_id": run_id,
            "agent_ids": agents,
            "phase": closed(mission["state"].as_str(), MISSION_PHASES),
            "step_counts": {"total": total, "completed": completed},
            "blocked_count": blocked,
            "revision": ident(mission["revision"].as_str()),
            "updated_at": timestamp(&mission["updated_at"]),
        }));
    }
    Ok(rows)
}

/// Current agents from the published roster (or this cut's cards without a refresher):
/// lifecycle, runtime state, current mission and run, updated-at. No workspace, checkout,
/// environment, ask, reason or conversation preview.
fn agent_rows(state: &AppState, _person: &str, index: u64, now: u128) -> anyhow::Result<Vec<Value>> {
    let store = &state.store;
    let mut agents = if store.agent_roster_refresher_running() {
        let Some((cut, cards, _)) = store.published_agent_roster(index, false) else {
            store.request_agent_roster_refresh();
            anyhow::bail!("the agents roster is still being prepared; retry shortly");
        };
        if cut < index {
            store.request_agent_roster_refresh();
        }
        (*cards).clone()
    } else {
        client_agent_resources_cached(store, false, index)?
    };
    overlay_agent_resources(store, &mut agents, &client_timestamp(now))?;
    Ok(agents
        .iter()
        .filter_map(|agent| {
            let id = ident(agent["id"].as_str()).filter(|id| id.starts_with("agent/"))?;
            let work = &agent["current_work"][0];
            Some(json!({
                "id": id,
                "lifecycle": agent["lifecycle"].as_str().map(|lifecycle| closed(Some(lifecycle), AGENT_LIFECYCLES)),
                "runtime_state": closed(agent["state"].as_str(), AGENT_STATES),
                "current_mission_id": ident(work["mission_id"].as_str()),
                "current_run_id": ident(work["mission_run_id"].as_str().or_else(|| agent["owner_run_id"].as_str())),
                "updated_at": timestamp(&agent["updated_at"]),
            }))
        })
        .collect())
}

/// Escaped JSON string bytes of `text`, so a budget holds for the encoded response.
fn encoded_len(text: &str) -> usize {
    text.chars()
        .map(|character| match character {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            character if u32::from(character) < 0x20 => 6,
            character => character.len_utf8(),
        })
        .sum()
}

fn offset(parameters: &BTreeMap<String, String>, name: &str) -> Result<usize, ApiError> {
    required(parameters, name)?
        .parse()
        .map_err(|_| validation(format!("`{name}` must be a byte offset")))
}

/// One exact UTF-8 byte range of one authenticated conversation text. The native record
/// is opened only if it fits the response budget, so no larger body is ever read. The
/// range must lie on character boundaries inside the text and fit the budget; it is never
/// widened, split or continued automatically. `next_range` names the next equal range.
async fn conversation(
    state: &AppState,
    parameters: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<Value, ApiError> {
    allow(parameters, CONVERSATION_FIELDS, max_bytes)?;
    let agent_id = required(parameters, "agent_id")?.to_owned();
    let session_id = required(parameters, "session_id")?.to_owned();
    let reference = required(parameters, "reference")?.to_owned();
    let revision = required(parameters, "revision")?.to_owned();
    let (start, end) = (offset(parameters, "start")?, offset(parameters, "end")?);
    if !agent_id.starts_with("agent/") || ident(Some(agent_id.as_str())).is_none() {
        return Err(validation("agent_id must name an agent"));
    }
    if start >= end {
        return Err(validation("the content range must be nonempty"));
    }
    if end - start > max_bytes {
        return Err(budget_exceeded("the content range exceeds the response budget"));
    }
    if conversation_session_id(state, &agent_id)? != session_id {
        return Err(stale("the session is not the agent's current conversation"));
    }
    let reader = state.clone();
    let (session, wanted) = (session_id.clone(), reference.clone());
    let (text, pinned) = crate::api::read_deadline::spawn_blocking(move || {
        super::super::conversation_blocks::assistant_text(
            &reader, &session, &wanted, max_bytes,
        )
    })
    .await
    .map_err(ApiError::internal)??;
    let pinned = match pinned {
        Value::String(revision) => revision,
        other => other.to_string(),
    };
    if pinned != revision {
        return Err(stale("the content revision changed; read the current revision"));
    }
    if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(validation(
            "the content range must lie on UTF-8 character boundaries within the text",
        ));
    }
    let slice = &text[start..end];
    if encoded_len(slice) > max_bytes {
        return Err(budget_exceeded("the encoded content range exceeds the response budget"));
    }
    let next_range = (end < text.len()).then(|| {
        let mut next = (end + (end - start)).min(text.len());
        while !text.is_char_boundary(next) {
            next += 1;
        }
        json!({"start": end, "end": next})
    });
    Ok(json!({
        "kind": "assistant-conversation-content",
        "agent_id": agent_id,
        "session_id": session_id,
        "reference": reference,
        "revision": revision,
        "start": start,
        "end": end,
        "text": slice,
        "returned_bytes": slice.len(),
        "truncated": end < text.len(),
        "next_range": next_range,
    }))
}

/// The current screen of one exactly named terminal incarnation and screen revision, as
/// text, cut on a character boundary to the budget. The shared screen is borrowed, never
/// rendered whole: only the returned characters are copied. One screen, never a stream.
async fn terminal(
    state: &AppState,
    parameters: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<Value, ApiError> {
    allow(parameters, TERMINAL_FIELDS, max_bytes)?;
    let agent_id = required(parameters, "agent_id")?;
    let terminal_id = required(parameters, "terminal_id")?;
    let incarnation_id = required(parameters, "incarnation_id")?;
    let screen_revision = required(parameters, "screen_revision")?;
    if !agent_id.starts_with("agent/") || ident(Some(agent_id)).is_none() {
        return Err(validation("agent_id must name an agent"));
    }
    if terminal_id != format!("terminal/{agent_id}") {
        return Err(validation("terminal_id must be the agent's terminal"));
    }
    let live = crate::api::read_deadline::query(
        &state.store,
        "/v1/client/assistant/terminal-content",
        || terminal_live_session(state, agent_id, Some(incarnation_id)),
    )?;
    if !live.terminal {
        return Err(validation("the requested runtime does not expose a terminal"));
    }
    let mut screens = crate::api::terminal_view::subscribe(
        &state.pty_root,
        &live.runtime_id,
        &live.incarnation_id,
    );
    let first = tokio::time::Instant::now() + TERMINAL_FIRST_SCREEN_TIMEOUT;
    let screen = crate::api::terminal_view::next_screen(&mut screens, None, first)
        .await
        .map_err(terminal_view_error)?
        .ok_or_else(|| terminal_unavailable("the terminal screen did not arrive"))?;
    if screen.revision() != screen_revision {
        return Err(stale("the terminal screen changed; read the current screen revision"));
    }
    let mut text = String::with_capacity(max_bytes.min(4 * 1024));
    let mut used = 0;
    let mut truncated = false;
    'lines: for (index, line) in screen.line_texts().enumerate() {
        let separator = if index == 0 { "" } else { "\n" };
        for character in separator.chars().chain(line.chars()) {
            let size = encoded_len(character.encode_utf8(&mut [0; 4]));
            if used + size > max_bytes {
                truncated = true;
                break 'lines;
            }
            used += size;
            text.push(character);
        }
    }
    Ok(json!({
        "kind": "assistant-terminal-content",
        "agent_id": agent_id,
        "terminal_id": terminal_id,
        "incarnation_id": incarnation_id,
        "screen_revision": screen_revision,
        "returned_bytes": text.len(),
        "text": text,
        "truncated": truncated,
    }))
}

#[cfg(test)]
#[path = "reads_tests.rs"]
pub mod tests;
