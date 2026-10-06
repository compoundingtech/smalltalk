use super::*;
use crate::store::owned_sets::{self as sets, Request as SetRequest};

pub(super) async fn preview(
    State(state): State<AppState>,
    Json(request): Json<SetRequest>,
) -> Result<Json<sets::Preview>, ApiError> {
    person_or_agent_actor(&request.actor, "invalid-set-actor")?;
    let intent = crate::graph::parse_owned_set_intent(&request.intent.kdl, &state.node)
        .map_err(ApiError::bad)?;
    let mut preview = state
        .store
        .owned_set_preview(&intent, &request.options)
        .map_err(ApiError::bad)?;
    if let Some(error) = publication_refusals(&state, &intent).await?.error() {
        preview.blockers.push(error.message);
    }
    for (subject, effect) in &preview.effects {
        let manual = effect.contains("pending (manual)");
        if !effect.starts_with("drain") && !manual {
            continue;
        }
        let actual = state
            .store
            .latest_actual_value(subject)
            .map_err(ApiError::internal)?;
        let incarnation = actual.as_ref().and_then(|a| a["incarnation_id"].as_str());
        let blocking = match incarnation {
            _ if manual => Vec::new(),
            Some(incarnation) => crate::suspension::blockers(&state.store, subject, incarnation)
                .map_err(ApiError::internal)?,
            None => vec!["runtime-unknown".into()],
        };
        preview.rollouts.insert(
            subject.clone(),
            json!({"action":if effect.contains("retire") {"retire"} else {"cutover"},
            "incarnation":incarnation,"blocking":blocking,
            "policy":if manual {json!({"mode":"manual"})} else {json!(preview.rollout)}}),
        );
    }
    Ok(Json(preview))
}

pub(super) async fn apply(
    State(state): State<AppState>,
    Json(request): Json<SetRequest>,
) -> Result<Json<Value>, ApiError> {
    person_or_agent_actor(&request.actor, "invalid-set-actor")?;
    let intent = crate::graph::parse_owned_set_intent(&request.intent.kdl, &state.node)
        .map_err(ApiError::bad)?;
    if let Some(error) = publication_refusals(&state, &intent).await?.error() {
        return Err(ApiError::bad(error));
    }
    let response = state
        .store
        .apply_owned_set(
            &intent,
            &request.options,
            &request.idempotency_key,
            &request.actor,
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    let set = state.store.owned_sets().map_err(ApiError::bad)?.into_iter()
        .find(|view| view.id == sets::subject(&request.options.set).unwrap())
        .map(|view| resource(&state, view)).transpose().map_err(ApiError::internal)?;
    Ok(Json(json!({"publication":response,"set":set})))
}

fn resource(state: &AppState, view: sets::View) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(&view)?;
    for (subject, blocker) in &view.deferred {
        value["blockers"].as_array_mut().unwrap().push(json!(format!(
            "{subject}: suspended ({}) — launch change deferred; republish after resume",
            blocker["reason"].as_str().unwrap_or("no reason provided"))));
    }
    let mut statuses = Vec::new();
    for (subject, member, retired) in state.store.owned_set_effective_members(&view)? {
        let status = state
            .store
            .status_at(Some(&subject), None, Some(state.store.index()?))?
            .subjects
            .into_iter()
            .find(|s| s.subject == subject);
        let mut launched = None;
        let incarnation = status
            .as_ref()
            .and_then(|s| s.actual.as_ref())
            .and_then(|a| a.get("incarnation_id"))
            .and_then(Value::as_str);
        for claim in state
            .store
            .observations_for(&subject, "runtime.action.succeeded")?
            .into_iter()
            .rev()
        {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            if fields["action"] == "start" && fields["incarnation_id"].as_str() == incarnation {
                launched = fields
                    .get("desired_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
        }
        let actual = status
            .as_ref()
            .and_then(|s| s.actual.as_ref())
            .and_then(|a| a.get("status"))
            .and_then(Value::as_str);
        let launch_current = launched.as_ref().is_some_and(|token| {
            state
                .store
                .launch_lineage(&subject)
                .is_ok_and(|lineage| lineage.contains(token))
        });
        let operation = state.store.rollout(&subject)?;
        let rollout_status = crate::rollout::status(&state.store, &subject)?;
        let manual_pending = rollout_status
            .as_ref()
            .is_some_and(|s| s["mode"] == "manual" && s["phase"] == "pending");
        let verified = operation.as_ref().is_none_or(|o| o.phase == "running");
        let running = actual == Some("running") && launch_current && verified;
        let phase = if view.deferred.contains_key(&subject) {
            "deferred"
        } else if !view.blockers.is_empty() {
            "blocked"
        } else if manual_pending {
            "pending"
        } else if let Some(operation) = &operation {
            operation.phase.as_str()
        } else if member.kind == "mission" || member.kind == "schedule" {
            if retired { "retired" } else { "published" }
        } else if retired && actual == Some("running") {
            "retirement-pending"
        } else if retired && matches!(actual, Some("stopped" | "exited" | "vanished" | "absent")) {
            "retired"
        } else if running {
            "running"
        } else if actual.is_none() {
            "unknown"
        } else {
            "pending"
        };
        statuses.push(json!({"subject":subject,"desired_token":member.claim,"launched_token":launched,"incarnation":incarnation,
            "retired":retired,"launch_current":launch_current,"rollout":phase,"operation":operation,
            "blocker":view.deferred.get(&subject),
            "rollout_mode":if manual_pending {Some("manual")} else {None},
            "publication_status":if manual_pending {Some("published, rollout pending (manual)")} else {None}}));
    }
    value["members_status"] = json!(statuses);
    let mut replicas = state
        .store
        .owned_set_replica_names()?
        .into_iter()
        .map(|name| json!({"host":name,"state":if name==state.node{"visible"}else{"unknown"}}))
        .collect::<Vec<_>>();
    if !replicas.iter().any(|r| r["host"] == state.node) {
        replicas.push(json!({"host":state.node,"state":"visible"}));
    }
    value["visibility"] =
        json!({"host":state.node,"state":"visible","replicas":replicas,"other_replicas":"unknown"});
    Ok(value)
}

// Match subject.definition: projection readers may inspect launch changes, but
// environment values require declaration scope. Proposals occur in three readback
// locations, including the receipt itself.
fn redact_proposals(value: &mut Value) {
    let redact = |blocker: &mut Value| {
        if let Some(desired) = blocker.pointer_mut("/proposed/desired") {
            crate::graph::redact_agent_env_values(desired);
        }
        if let Some(environment) = blocker.pointer_mut("/proposed/member/environment")
            .and_then(Value::as_object_mut)
        {
            for value in environment.values_mut() {
                *value = json!("<redacted>");
            }
        }
    };
    for path in ["/deferred", "/receipt/deferred"] {
        if let Some(blockers) = value.pointer_mut(path).and_then(Value::as_object_mut) {
            for blocker in blockers.values_mut() { redact(blocker); }
        }
    }
    if let Some(members) = value["members_status"].as_array_mut() {
        for member in members {
            if let Some(blocker) = member.get_mut("blocker") { redact(blocker); }
        }
    }
}

pub(super) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let show_env_values = session.allows("read.declarations");
    client_snapshot_page(&state, snapshot, "sets", &query, move |state, _| {
        state
            .store
            .owned_sets()?
            .into_iter()
            .map(|v| {
                let mut value = resource(state, v)?;
                if !show_env_values { redact_proposals(&mut value); }
                Ok(value)
            })
            .collect()
    })
    .await
}

#[derive(Default, Deserialize)]
pub(super) struct DetailQuery {
    sha: Option<String>,
}

pub(super) async fn get(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<DetailQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let (snapshot, value) = blocking_store(move || {
        state.store.read_snapshot(|index| {
            Ok((
                client_snapshot_at(&state, index),
                detail(&state, &id, query.sha),
            ))
        })
    })
    .await?;
    let mut value = value?;
    if !session.allows("read.declarations") { redact_proposals(&mut value); }
    Ok((Extension(snapshot), Json(value)))
}

fn detail(state: &AppState, id: &str, sha: Option<String>) -> Result<Value, ApiError> {
    let subject = sets::subject(id).map_err(ApiError::bad)?;
    let selected = state
        .store
        .owned_sets()
        .map_err(ApiError::bad)?
        .into_iter()
        .find(|v| v.id == subject)
        .ok_or_else(|| ApiError::not_found(format!("set {subject} does not exist")))?;
    if let Some(sha) = sha {
        let receipts = state
            .store
            .owned_set_history(id)
            .map_err(ApiError::bad)?
            .into_iter()
            .filter(|v| v.receipt.source.sha == sha)
            .collect::<Vec<_>>();
        if receipts.is_empty() {
            return Err(ApiError::not_found(format!(
                "commit {sha} has no receipt in {subject}"
            )));
        }
        let superseded = selected.receipt.source.sha != sha;
        let mut value = resource(state, selected).map_err(ApiError::internal)?;
        let running = !superseded
            && value["blockers"].as_array().is_some_and(Vec::is_empty)
            && value["members_status"].as_array().is_some_and(|members| {
                members.iter().all(|m| {
                    matches!(
                        m["rollout"].as_str(),
                        Some("running" | "published" | "retired")
                    )
                })
            });
        let satisfied = !superseded
            && value["blockers"].as_array().is_some_and(Vec::is_empty)
            && value["members_status"].as_array().is_some_and(|members| {
                members.iter().all(|m| {
                    matches!(
                        m["rollout"].as_str(),
                        Some("running" | "published" | "retired")
                    ) || (m["rollout"] == "pending" && m["rollout_mode"] == "manual")
                })
            });
        value["commit_status"] = json!({"sha":sha,"published":true,"visible_local":true,
            "superseded":superseded,"running":running,"satisfied":satisfied,"receipts":receipts});
        return Ok(value);
    }
    resource(state, selected).map_err(ApiError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_apply_guard_readback_redacts_every_environment_copy() {
        let proposed = json!({"desired":{"name":"agent","children":[
            {"name":"env","children":[{"name":"EXAMPLE_TOKEN","arguments":["example-private-value"]}]}]},
            "member":{"environment":{"EXAMPLE_TOKEN":"example-private-value"}}});
        let blocker = json!({"reason":"Paused for the winter","proposed":proposed});
        let mut value = json!({"deferred":{"agent/garden/orchard":blocker},
            "receipt":{"deferred":{"agent/garden/orchard":blocker}},
            "members_status":[{"blocker":blocker}]});
        redact_proposals(&mut value);
        assert!(!value.to_string().contains("example-private-value"));
        assert_eq!(value["deferred"]["agent/garden/orchard"]["reason"], "Paused for the winter");
        assert_eq!(value["members_status"][0]["blocker"]["proposed"]["member"]["environment"]["EXAMPLE_TOKEN"], "<redacted>");
        assert_eq!(value["receipt"]["deferred"]["agent/garden/orchard"]["proposed"]["desired"]["children"][0]["children"][0]["arguments"][0], "<redacted>");
    }
}
