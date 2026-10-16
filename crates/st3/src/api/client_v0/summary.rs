//! Tiny current summary on the existing protocol. Native reads are authorized in
//! one snapshot; the contract stays the same when the shared IVM source takes over.
use super::*;
use crate::store::summary_list::{SummaryInputs, SummaryMachines, SummaryShared};

#[derive(Debug)]
struct RosterNotPublished;

impl std::fmt::Display for RosterNotPublished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the agents roster is still being prepared; retry shortly")
    }
}

impl std::error::Error for RosterNotPublished {}

pub(super) fn working(agent: &Value) -> bool {
    agent["state"] == "running"
        && agent["harness_state"] == "working"
        && agent["fault"].is_null()
        && agent["delivery"]["state"] != "stale"
}
/// A mission that is really running or about to: stuck, paused and finished runs, and ones that
/// only wait on a person, are counted elsewhere.
pub(super) fn active(mission: &st3_client::Mission) -> bool {
    !mission.system && st3_ui_model::missions::row_word(mission, &[]).is_active()
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
    let attention = state.store.attention_snapshot(person.as_deref(), now)?;
    // What needs the person is what waits on their answer: an update asks nothing.
    let alerts = attention.iter().filter(|item| item.is_alert()).count();
    let shared = shared(state, session, request, snapshot, now, windows, commits, None)?;
    Ok(vec![row(person.as_deref(), alerts, &shared)?])
}

/// What every selection's summary shares at `snapshot`: each mission and whether it is active,
/// the working agents and the machine counts, of `machines` when they were read at `snapshot`.
/// No count here depends on the selected person.
#[allow(clippy::too_many_arguments)]
fn shared(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
    snapshot: &ClientSnapshot,
    now: u128,
    windows: Option<&collection_windows::Windows>,
    commits: Option<u64>,
    machines: Option<Arc<Vec<Value>>>,
) -> anyhow::Result<SummaryShared> {
    let store = &state.store;
    let compute = || {
        let (missions, deadline) = store.client_summary_missions(now)?;
        Ok((
            vec![
                json!({"evaluated_at":now.to_string(),"valid_until":deadline.map(|d| d.to_string()),"missions":missions
                .iter()
                .map(|m| json!({"id":m.header.id,"system":m.system,"active":active(m)}))
                .collect::<Vec<_>>()}),
            ],
            false,
            deadline,
        ))
    };
    // Mission active membership cannot depend on which agent owns a ready queue: a step that
    // waits its turn counts. Native runtime/harness activity therefore reuses this lean
    // mission input instead of rereading every step.
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
    let missions = missions
        .first()
        .and_then(|v| v["missions"].as_array())
        .into_iter()
        .flatten()
        .map(|m| {
            (
                m["id"].as_str().unwrap_or_default().to_owned(),
                m["system"] == true,
                m["active"] == true,
            )
        })
        .collect();
    // A daemon's reads never fold the roster: count from its newest publication, and say which.
    let mut agents_as_of = None;
    let mut agents = if store.agent_roster_refresher_running() {
        let Some((cut, cards, published_at)) = store.published_agent_roster(snapshot.store_index, false)
        else {
            store.request_agent_roster_refresh();
            return Err(RosterNotPublished.into());
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
    let working_agents = agents.iter().filter(|agent| working(agent)).count();
    let machines = match machines {
        Some(machines) => machines,
        None => Arc::new(machine_summary_resources(state, snapshot, session)?),
    };
    let machines = machine_counts(&machines, &agents, &client_host_id(&state.node), now);
    Ok(SummaryShared {
        cut: snapshot.store_index,
        evaluated_at_unix_ms: now,
        published_at_unix_ms: client_now_ms(),
        missions,
        working_agents,
        machines,
        agents_as_of,
    })
}

/// One selection's summary row: what needs `person` (its `alerts`) beside the counts every
/// selection shares.
fn row(person: Option<&str>, alerts: usize, shared: &SummaryShared) -> anyhow::Result<Value> {
    let active = shared
        .missions
        .iter()
        .filter(|(_, system, active)| !system && *active)
        .count();
    let mut value = json!({"person_id":person,"alerts":alerts,"needs_you":alerts,"working_agents":shared.working_agents,"active_missions":active,"machines":shared.machines});
    value["revision"] = json!(smallclaims::hash::canonical_hash(&value)?);
    value["id"] = json!("summary/current");
    value["kind"] = json!("summary");
    value["updated_at"] = json!(client_timestamp(shared.evaluated_at_unix_ms));
    // Outside the revision: a newer roster with the same counts changes nothing a client shows.
    if let Some(as_of) = &shared.agents_as_of {
        value["agents_as_of"] = as_of.clone();
    }
    Ok(value)
}

/// What needs `person` (every person, for none) in published attention rows, counted as from
/// the attention items they were made of: the alerts.
pub(super) fn published_attention_alerts(rows: &[Value], person: Option<&str>) -> usize {
    rows.iter()
        .filter(|row| person.is_none_or(|person| row["person_id"] == person))
        .filter(|row| row["alert"] == true)
        .count()
}

/// The row of `person` from the newest shared counts and attention publication, with the
/// shared counts' cut and publication time: what the refresher publishes for that selection.
/// `None` until both are published. Memory only: a window that opens a selection is served at
/// once, before the refresher publishes it.
pub(in crate::api) fn published_row(
    store: &Store,
    person: Option<&str>,
) -> Option<(u64, u128, Value)> {
    let (shared, _) = store.summary_shared()?;
    let attention = store.newest_attention_list().1?;
    let alerts = published_attention_alerts(&attention.rows, person);
    let row = row(person, alerts, &shared).ok()?;
    Some((shared.cut, shared.published_at_unix_ms, row))
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

/// How often the shared counts are computed again with nothing else changed: delivery
/// staleness, machine recency and mission deadlines read the clock.
const PUBLISHED_PERIOD_MS: u128 = 30_000;

/// Publish the summary row of every selection a window read within `idle_ms`. The counts no
/// person changes are computed once for all of them, in one short snapshot, and again only when
/// their inputs (cut, roster, clock period) change; each selection then counts what needs its
/// person from the newest attention publication, in memory. As the daemon starts the shared
/// counts are computed with no selection until they first succeed (the complete roster they
/// count agents from may come later), so the first window is served at once. If they
/// cannot be computed the previous ones stay published. Before the first successful computation,
/// selections stay pending only while the agent roster has not published; other failures let
/// their windows read for themselves.
/// With `waiting_only`, only the selections a window waits for, and the shared counts only if
/// there are none yet. Returns whether any selection's counts changed.
pub(in crate::api) fn refresh_published(
    state: &AppState,
    idle_ms: u64,
    waiting_only: bool,
) -> anyhow::Result<bool> {
    let store = &state.store;
    let (generation, selections) = store.summary_selections(idle_ms, waiting_only);
    let starting = store.summary_shared_starting();
    if selections.is_empty() && !starting {
        return Ok(false);
    }
    let before = store.summary_shared();
    let roster = *store.subscribe_agent_roster().borrow();
    let computed = store.read_snapshot(|index| {
        let now = client_now_ms();
        let inputs = SummaryInputs {
            cut: index,
            roster,
            period: now / PUBLISHED_PERIOD_MS,
        };
        if before
            .as_ref()
            .is_some_and(|(_, held)| waiting_only || *held == inputs)
        {
            return Ok(None);
        }
        // No shared count depends on the session: a read-only local session reads them all.
        let session = ClientSession::local(None).map_err(|error| anyhow::anyhow!(error.message))?;
        let request: CollectionSubscribe = serde_json::from_value(json!({
            "kind":"subscribe", "id":"summary", "collection":"summary", "limit":1,
        }))?;
        let snapshot = client_snapshot_at(state, index);
        let machines = published_machines(state, generation, &session, &snapshot, inputs.period)?;
        Ok(Some((shared(state, &session, &request, &snapshot, now, None, None, Some(machines))?, inputs)))
    });
    let mut waiting_for_roster = false;
    let shared = match computed {
        Ok(None) => before.map(|(shared, _)| shared),
        Ok(Some((shared, inputs))) => {
            let shared = Arc::new(shared);
            store.publish_summary_shared(generation, shared.clone(), inputs);
            store.start_summary_shared();
            store.note_summary_shared_failed(false);
            if starting {
                tracing::debug!(
                    event = "summary.startup",
                    outcome = "published",
                    selections = selections.len(),
                    cut = shared.cut,
                    "summary startup shared counts published"
                );
            }
            Some(shared)
        }
        Err(error) => {
            waiting_for_roster = starting && error.is::<RosterNotPublished>();
            // Expected dependency waits do not hide a later real error in the same startup.
            if waiting_for_roster {
                store.note_summary_shared_failed(false);
                tracing::debug!(
                    event = "summary.shared_counts_failed",
                    outcome = "pending",
                    reason = "roster_not_published",
                    selections = selections.len(),
                    %error,
                    "summary waiting for initial roster publication"
                );
            } else if store.note_summary_shared_failed(true) {
                tracing::warn!(
                    event = "summary.shared_counts_failed",
                    outcome = "previous_or_fallback",
                    reason = "computation_error",
                    selections = selections.len(),
                    %error,
                    "summary shared counts unavailable"
                );
            }
            before.map(|(shared, _)| shared)
        }
    };
    let mut changed = false;
    let Some(shared) = shared else {
        // Only the expected initial roster dependency is a wait. Other errors must preserve
        // the native fallback even if no shared computation has succeeded yet.
        if waiting_for_roster {
            return Ok(false);
        }
        // Nothing to count from: every selection's windows read for themselves until there is.
        for person in &selections {
            changed |= store.publish_summary_row(generation, person, None);
        }
        if changed {
            store.publish_collection_view("summary");
        }
        return Ok(changed);
    };
    // Before attention's first publication there is nothing to count a person's alerts from;
    // its windows wait for it, as attention windows do.
    let Some(attention) = store.newest_attention_list().1 else {
        return Ok(false);
    };
    for person in &selections {
        let alerts = published_attention_alerts(&attention.rows, person.as_deref());
        let row = crate::store::summary_list::SummaryRow {
            cut: shared.cut,
            published_at_unix_ms: shared.published_at_unix_ms,
            row: row(person.as_deref(), alerts, &shared)?,
        };
        changed |= store.publish_summary_row(generation, person, Some(row));
    }
    if changed {
        store.publish_collection_view("summary");
    }
    Ok(changed)
}

/// The machines the shared counts count at `snapshot`: the ones read before while they still
/// hold (`Store::summary_machines_hold`), else read again. A runtime with claims since is reduced
/// again alone: the machines hold while the current runtimes run on the same hosts as before,
/// whichever runtimes those are. They are read again each clock `period` too, since a peer's replication
/// successes change them with no claim, as do a new conflicting operation and the fleet's
/// membership, compared each time.
pub(in crate::api) fn published_machines(
    state: &AppState,
    generation: u64,
    session: &ClientSession,
    snapshot: &ClientSnapshot,
    period: u128,
) -> anyhow::Result<Arc<Vec<Value>>> {
    let store = &state.store;
    let index = snapshot.store_index;
    let conflicts = store.conflicting_operations()?;
    let fleet = store.fleet_view_for_client()?;
    let held = store.summary_machines().filter(|held| {
        held.period == period && held.conflicts == conflicts && held.fleet == fleet
    });
    let kept = match held {
        Some(held) => match store.summary_machines_hold(held.cut, index)? {
            Some(changed) if changed.is_empty() => Some((held.machines, held.runtimes)),
            Some(changed) => {
                let status = store.status_for_subject_names_at(changed.clone(), index, false, crate::store::ClaimsReduction::Runtime)?;
                let hosts = runtime_resources_from_status(state, snapshot, session, status)?
                    .into_iter()
                    .filter_map(|runtime| {
                        Some((
                            runtime["owner_id"].as_str()?.to_owned(),
                            runtime["owner_host_id"].as_str()?.to_owned(),
                        ))
                    })
                    .collect::<BTreeMap<_, _>>();
                let mut runtimes = (*held.runtimes).clone();
                for runtime in changed {
                    match hosts.get(&runtime) {
                        Some(host) => runtimes.insert(runtime, host.clone()),
                        None => runtimes.remove(&runtime),
                    };
                }
                // The machines are the same hosts while the runtimes' hosts are.
                (held.runtimes.values().collect::<BTreeSet<_>>()
                    == runtimes.values().collect::<BTreeSet<_>>())
                .then(|| (held.machines, Arc::new(runtimes)))
            }
            None => None,
        },
        None => None,
    };
    let (machines, runtimes) = match kept {
        Some(kept) => kept,
        None => {
            let (machines, runtimes) = machine_summary_resources_and_hosts(state, snapshot, session)?;
            (Arc::new(machines), Arc::new(runtimes))
        }
    };
    store.keep_summary_machines(
        generation,
        SummaryMachines { cut: index, period, conflicts, fleet, machines: machines.clone(), runtimes },
    );
    Ok(machines)
}

#[cfg(test)]
#[path = "summary/tests.rs"]
mod tests;
