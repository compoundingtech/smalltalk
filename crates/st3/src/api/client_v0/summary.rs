//! Tiny current summary on the existing protocol. Native reads are authorized in
//! one snapshot; the contract stays the same when the shared IVM source takes over.
use super::*;
use st3_ui_model::missions::{Display, Word};

pub(super) fn home_kind(kind: &str) -> bool {
    matches!(
        kind,
        "human-gate"
            | "launch-approval"
            | "revision-approval"
            | "person-step"
            | "agent-request"
            | "harness-prompt"
    ) || kind.starts_with("custom.")
}
pub(super) fn working(agent: &Value) -> bool {
    agent["state"] == "running"
        && agent["harness_state"] == "working"
        && agent["fault"].is_null()
        && agent["delivery"]["state"] != "stale"
}
pub(super) fn active(mission: &st3_ui_model::missions::Mission) -> bool {
    !mission.system
        && matches!(
            mission.word,
            Word::Working
                | Word::Queued
                | Word::Decision
                | Word::Stalled
                | Word::Unstaffed
                | Word::Unclaimed
        )
}

pub(super) fn native(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
    snapshot: &ClientSnapshot,
    now: u128,
    windows: Option<&collection_windows::Windows>,
    commits: Option<u64>,
) -> anyhow::Result<Vec<Value>> {
    let person = person_filter(session, request.person.as_deref())
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let store = &state.store;
    let attention = store.attention_snapshot(person.as_deref(), now)?;
    let needs = attention
        .iter()
        .filter(|item| home_kind(&item.kind))
        .count();
    let gates = attention
        .iter()
        .filter(|item| item.kind == "human-gate")
        .filter_map(|item| item.mission.clone())
        .collect::<BTreeSet<_>>();
    let compute = || {
        let (missions, deadline) = store.client_summary_missions(now)?;
        let semantics = st3_ui_model::missions::adapt(
            missions.iter(),
            std::iter::empty(),
            std::iter::empty(),
            "",
            &Display {
                mission_label: &|m| m.header.id.clone(),
                agent_label: &|a| a.name.clone(),
                age_label: &|_, _| String::new(),
                clean_text: &str::to_owned,
            },
        );
        Ok((
            vec![
                json!({"evaluated_at":now.to_string(),"valid_until":deadline.map(|d| d.to_string()),"missions":semantics
                .into_iter()
                .map(|m| json!({"id":m.id,"system":m.system,"active":active(&m)}))
                .collect::<Vec<_>>()}),
            ],
            false,
        ))
    };
    // Mission active membership cannot depend on which agent owns a ready queue:
    // unstaffed, queued and unclaimed all count. Native runtime/harness activity
    // therefore reuses this lean mission input instead of rereading every step.
    let missions = if let Some(windows) = windows {
        let mut internal = request.clone();
        internal.collection = "summary-missions".into();
        internal.limit = None;
        let prepared = windows.prepare(state, session, &internal);
        windows
            .read(
                state,
                session,
                &internal,
                collection_windows::ReadFence {
                    index: snapshot.store_index,
                    now,
                    commits: commits.expect("summary window commit fence"),
                    prepared,
                },
                compute,
            )?
            .0
    } else {
        compute()?.0
    };
    let active = missions
        .first()
        .and_then(|v| v["missions"].as_array())
        .into_iter()
        .flatten()
        .filter(|m| {
            m["system"] != true
                && (m["active"] == true || gates.contains(m["id"].as_str().unwrap_or_default()))
        })
        .count();
    // A daemon's reads never fold the roster: count from its newest publication, and say which.
    let mut agents_as_of = None;
    let mut agents = if store.agent_roster_refresher_running() {
        let Some((cut, cards, published_at)) = store.published_agent_roster(snapshot.store_index, false)
        else {
            store.request_agent_roster_refresh();
            anyhow::bail!("the agents roster is still being prepared; retry shortly");
        };
        if cut < snapshot.store_index {
            store.request_agent_roster_refresh();
        }
        agents_as_of = Some(json!({"store_index": cut, "published_at": client_timestamp(published_at)}));
        (*cards).clone()
    } else {
        client_agent_resources_cached(store, false, snapshot.store_index)?
    };
    overlay_agent_resources(store, &mut agents, &client_timestamp(now))?;
    let working = agents.iter().filter(|agent| working(agent)).count();
    let machines = machine_summary_resources(state, snapshot, session)?;
    let machine_counts = machine_counts(&machines, &agents, &client_host_id(&state.node), now);
    let mut value = json!({"person_id":person,"needs_you":needs,"working_agents":working,"active_missions":active,"machines":machine_counts});
    value["revision"] = json!(smallclaims::hash::canonical_hash(&value)?);
    value["id"] = json!("summary/current");
    value["kind"] = json!("summary");
    value["updated_at"] = json!(client_timestamp(now));
    // Outside the revision: a newer roster with the same counts changes nothing a client shows.
    if let Some(as_of) = agents_as_of {
        value["agents_as_of"] = as_of;
    }
    Ok(vec![value])
}

pub(super) fn machine_counts(
    machines: &[Value],
    agents: &[Value],
    gateway: &str,
    now: u128,
) -> Value {
    let mut counts = [0usize; 3];
    for machine in machines {
        let host = machine["host_id"].as_str().unwrap_or_default();
        let direct =
            host == gateway || matches!(machine["state"].as_str(), Some("local" | "reachable"));
        let recent = machine["transports"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["last_success_at"].as_str())
            .chain(
                agents
                    .iter()
                    .filter(|a| a["host_id"] == host)
                    .filter_map(|a| a["last_activity_at"].as_str()),
            )
            .filter_map(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .any(|at| {
                i128::try_from(now)
                    .is_ok_and(|now| now - i128::from(at.timestamp_millis()) < 300_000)
            });
        counts[if direct {
            0
        } else if recent {
            1
        } else {
            2
        }] += 1;
    }
    json!({"connected":counts[0],"indirect":counts[1],"offline":counts[2]})
}

pub(super) fn retain_timestamp(items: &mut [Value], previous: &BTreeMap<String, Value>) {
    for item in items {
        if let Some(old) = item["id"].as_str().and_then(|id| previous.get(id))
            && old["revision"] == item["revision"]
            && old["person_id"] == item["person_id"]
        {
            item["updated_at"] = old["updated_at"].clone();
            if !old["agents_as_of"].is_null() {
                item["agents_as_of"] = old["agents_as_of"].clone();
            }
        }
    }
}

#[cfg(test)]
#[path = "summary/tests.rs"]
mod tests;
