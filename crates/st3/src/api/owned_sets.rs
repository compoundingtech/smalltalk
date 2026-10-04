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
    if preview.rollout.is_some() {
        for (subject, effect) in &preview.effects {
            if !effect.starts_with("drain") {
                continue;
            }
            let actual = state
                .store
                .latest_actual_value(subject)
                .map_err(ApiError::internal)?;
            let incarnation = actual.as_ref().and_then(|a| a["incarnation_id"].as_str());
            let blocking = match incarnation {
                Some(incarnation) => {
                    crate::suspension::blockers(&state.store, subject, incarnation)
                        .map_err(ApiError::internal)?
                }
                None => vec!["runtime-unknown".into()],
            };
            preview.rollouts.insert(
                subject.clone(),
                json!({"action":if effect.contains("retire") {"retire"} else {"cutover"},
                "incarnation":incarnation,"blocking":blocking,"policy":preview.rollout}),
            );
        }
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
    Ok(Json(
        json!({"publication":response,"set":state.store.owned_sets().map_err(ApiError::bad)?.into_iter().find(|v|v.id==sets::subject(&request.options.set).unwrap())}),
    ))
}

fn resource(state: &AppState, view: sets::View) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(&view)?;
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
        let verified = operation.as_ref().is_none_or(|o| o.phase == "running");
        let running = actual == Some("running") && launch_current && verified;
        let phase = if !view.blockers.is_empty() {
            "blocked"
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
            "retired":retired,"launch_current":launch_current,"rollout":phase,"operation":operation}));
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

pub(super) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    client_snapshot_page(&state, snapshot, "sets", &query, |state, _| {
        state
            .store
            .owned_sets()?
            .into_iter()
            .map(|v| resource(state, v))
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
    Ok((Extension(snapshot), Json(value?)))
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
        value["commit_status"] = json!({"sha":sha,"published":true,"visible_local":true,
            "superseded":superseded,"running":running,"satisfied":running,"receipts":receipts});
        return Ok(value);
    }
    resource(state, selected).map_err(ApiError::internal)
}
