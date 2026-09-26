use super::*;
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use std::collections::BTreeSet;

const TERMINAL_SUBPROTOCOL: &str = "st3.client.terminal.v0";
const TERMINAL_CAPABILITY_PROTOCOL_PREFIX: &str = "st3.cap.";
const TERMINAL_MAX_LINES: usize = 200;
const TERMINAL_MAX_LINE_BYTES: usize = 4_096;
const LOCAL_PERSON_HEADER: &str = "x-st3-person";

const ALL_SCOPES: &[&str] = &[
    "read.projections",
    "terminal.read",
    "terminal.control",
    "control.attention",
    "control.messages",
    "control.launches",
    "control.missions",
    "control.work",
    "control.runtimes",
    "control.pairing",
];
const LIMITED_PAIRING_SCOPES: &[&str] = &[
    "read.projections",
    "terminal.read",
    "control.attention",
    "control.launches",
];
const ACTIONS: &[&str] = &[
    "attention.resolve",
    "review.approve",
    "review.reject",
    "message.send",
    "message.read",
    "message.close",
    "launch.create",
    "launch.revise",
    "launch.preview",
    "launch.approve",
    "launch.cancel",
    "mission.start",
    "mission.revise",
    "mission.approve-revision",
    "mission.cancel-revision",
    "mission.cancel",
    "session.import",
    "work.claim",
    "work.renew",
    "work.progress",
    "work.complete",
    "work.fail",
    "work.release",
    "work.publish-mission",
    "runtime.stop",
    "runtime.restart",
    "runtime.reset",
    "runtime.context-clear",
    "runtime.signal",
    "terminal.input",
    "terminal.resize",
    "terminal.attach",
    "terminal.detach",
    "pairing.revoke",
];
const AVAILABLE_ACTIONS: &[&str] = &[
    "attention.resolve",
    "review.approve",
    "review.reject",
    "message.send",
    "message.read",
    "message.close",
    "launch.create",
    "launch.revise",
    "launch.preview",
    "launch.approve",
    "launch.cancel",
    "mission.start",
    "mission.approve-revision",
    "mission.cancel-revision",
    "mission.cancel",
    "session.import",
    "work.claim",
    "work.renew",
    "work.progress",
    "work.complete",
    "work.fail",
    "work.release",
    "runtime.context-clear",
    "runtime.signal",
    "terminal.input",
    "terminal.resize",
    "terminal.attach",
    "terminal.detach",
    "pairing.revoke",
];

#[derive(Clone, Debug)]
pub(super) struct ClientSession {
    /// The credential/session identity used for audit and idempotency isolation.
    pub(super) actor: String,
    /// The concrete graph person whose explicitly delegated authority is exercised.
    pub(super) authority_actor: String,
    pub(super) transport: &'static str,
    scopes: std::collections::BTreeSet<String>,
}

impl ClientSession {
    fn local(person: Option<&str>) -> Result<Self, ApiError> {
        if person.is_some_and(|person| {
            !person.starts_with("person/") || person.matches('/').count() != 1
        }) {
            return Err(forbidden(
                "the trusted Unix client must identify one concrete person",
            ));
        }
        let Some(person) = person else {
            return Ok(Self {
                actor: "client/local/read-only".into(),
                authority_actor: "client/local/read-only".into(),
                transport: "unix",
                scopes: ["read.projections", "terminal.read"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            });
        };
        Ok(Self {
            actor: person.into(),
            authority_actor: person.into(),
            transport: "unix",
            scopes: ALL_SCOPES.iter().map(|scope| (*scope).to_owned()).collect(),
        })
    }

    fn pairing() -> Self {
        Self {
            actor: "client/pairing/completion".into(),
            authority_actor: "client/pairing/completion".into(),
            transport: "fabric-loopback",
            scopes: std::collections::BTreeSet::new(),
        }
    }

    fn allows(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

fn session_claim_actor(session: &ClientSession) -> String {
    if session.authority_actor.starts_with("person/") {
        session.authority_actor.clone()
    } else {
        "requester".into()
    }
}

pub(super) fn capabilities(session: &ClientSession) -> Vec<Value> {
    let mut capabilities = ALL_SCOPES
        .iter()
        .map(|scope| {
            json!({
                "id": scope,
                "version": 0,
                "state": if session.allows(scope) { "granted" } else { "ungranted" }
            })
        })
        .collect::<Vec<_>>();
    capabilities.extend(ACTIONS.iter().map(|action| {
        let scope = action_scope(action).expect("registered client action has a scope");
        let state = if !AVAILABLE_ACTIONS.contains(action) {
            "unavailable"
        } else if session.allows(scope) {
            "granted"
        } else {
            "ungranted"
        };
        json!({ "id": action, "version": 0, "state": state })
    }));
    capabilities
}

fn forbidden(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: message.into(),
        details: Box::default(),
    }
}

pub(super) fn fabric_boundary_forbidden() -> ApiError {
    forbidden("the client gateway exposes only the authenticated client-v0 boundary")
}

fn validation(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation-failed".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn stale(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        code: "stale-fence".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn credential_digest(credential: &str) -> String {
    hex::encode(Sha256::digest(credential.as_bytes()))
}

pub(super) fn authenticate(
    state: &AppState,
    request: &Request<Body>,
    transport: &'static str,
) -> Result<ClientSession, ApiError> {
    let Some(value) = request.headers().get(AUTHORIZATION) else {
        if transport == "unix" {
            let person = request
                .headers()
                .get(LOCAL_PERSON_HEADER)
                .and_then(|value| value.to_str().ok());
            return ClientSession::local(person);
        }
        let pairing_completion = request.method() == axum::http::Method::POST
            && request.uri().path().starts_with("/v1/client/pairings/")
            && request.uri().path().ends_with("/complete");
        return pairing_completion
            .then(ClientSession::pairing)
            .ok_or_else(|| forbidden("the Fabric-loopback client credential is required"));
    };
    let value = value
        .to_str()
        .map_err(|_| forbidden("the client authorization header is malformed"))?;
    let credential = value
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| forbidden("the client authorization scheme must be Bearer"))?;
    let digest = credential_digest(credential);
    let pairings = state
        .store
        .claims_for_kind_at("custom.client.pairing-completed", None, true, 10_000)
        .map_err(ApiError::internal)?;
    let paired = pairings.claims.iter().find(|claim| {
        claim
            .body
            .pointer("/fields/credential_hash")
            .and_then(Value::as_str)
            == Some(digest.as_str())
    });
    let Some(paired) = paired else {
        return Err(forbidden("the client credential is unknown or expired"));
    };
    let revoked = state
        .store
        .claims_for_subject_kind_at(
            &paired.subject,
            "custom.client.pairing-revoked",
            None,
            true,
            1,
        )
        .map_err(ApiError::internal)?
        .claims
        .first()
        .is_some_and(|claim| claim.store_index > paired.store_index);
    let expires_at = paired
        .body
        .pointer("/fields/expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    if revoked || expires_at <= client_now_ms() {
        return Err(forbidden("the client credential was revoked or expired"));
    }
    let actor = paired
        .body
        .pointer("/fields/session_actor")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("a paired client has no derived actor"))?;
    let authority_actor = paired
        .body
        .pointer("/fields/person_id")
        .and_then(Value::as_str)
        .filter(|actor| actor.starts_with("person/") && actor.matches('/').count() == 1)
        .ok_or_else(|| ApiError::internal("a paired client has no concrete delegated person"))?;
    let scopes = paired
        .body
        .pointer("/fields/scopes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let session = ClientSession {
        actor: actor.into(),
        authority_actor: authority_actor.into(),
        transport,
        scopes,
    };
    if request.method() == axum::http::Method::GET {
        let scope = if request.uri().path().starts_with("/v1/client/terminals/") {
            "terminal.read"
        } else {
            "read.projections"
        };
        require_scope(&session, scope)?;
    }
    Ok(session)
}

fn require_scope(session: &ClientSession, scope: &str) -> Result<(), ApiError> {
    if session.allows(scope) {
        Ok(())
    } else {
        Err(forbidden(format!(
            "the authenticated client session is not granted `{scope}`"
        )))
    }
}

pub(super) fn person_filter(
    session: &ClientSession,
    requested: Option<&str>,
) -> Result<Option<String>, ApiError> {
    if session.authority_actor.starts_with("person/") {
        if requested.is_some_and(|person| person != session.authority_actor) {
            return Err(forbidden(
                "the authenticated client cannot read another person's private projection",
            ));
        }
        return Ok(Some(session.authority_actor.clone()));
    }
    Ok(requested.map(str::to_owned))
}

fn mission_visualization(
    store: &Store,
    mission_id: &str,
    revision: &str,
    state: &str,
) -> anyhow::Result<Option<Value>> {
    let Some(mission) =
        store.mission_spec(mission_id.trim_start_matches("mission/"), Some(revision))?
    else {
        return Ok(None);
    };
    let nodes = mission
        .display_order
        .iter()
        .filter_map(|id| mission.steps.get(id))
        .map(|step| json!({
            "id": format!("step/{}", step.path), "kind": "step",
            "label": step.title.as_deref().unwrap_or(&step.id), "path": step.path,
            "goals": step.goals, "constraints": step.constraints,
            "assignment": step.work_selector, "timeout_ms": step.timeout_ms,
            "retry": step.retry, "gates": step.gates, "loop": step.loop_spec,
            "resources": step.documents, "source_references": step.documents,
            "runtime": { "state": state, "attempt": null, "progress": null, "blockers": [], "attention": [], "errors": [] }
        }))
        .collect::<Vec<_>>();
    let edges = mission.steps.values().flat_map(|step| step.dependencies.iter().map(move |dependency| match dependency {
        crate::model::DependencySpec::Step { step: source, .. } => json!({"id": format!("edge/{source}/{}", step.path), "kind":"dependency", "from":format!("step/{source}"), "to":format!("step/{}", step.path), "gate":dependency}),
        crate::model::DependencySpec::Predicate { .. } => json!({"id":format!("edge/predicate/{}", step.path), "kind":"gate", "from":null, "to":format!("step/{}", step.path), "gate":dependency}),
    })).collect::<Vec<_>>();
    let timeline = mission.display_order.iter().enumerate().filter_map(|(ordinal, id)| mission.steps.get(id).map(|step| json!({"id":format!("timeline/{}", step.path), "node":format!("step/{}", step.path), "ordinal":ordinal, "dependencies":step.dependencies, "timeout_ms":step.timeout_ms}))).collect::<Vec<_>>();
    let mut decisions = Vec::new();
    for session in store
        .planning_sessions(true)?
        .into_iter()
        .filter(|session| {
            session.mission == mission_id
                || format!("mission/{}", session.mission) == mission_id
                || session.mission == mission_id.trim_start_matches("mission/")
        })
    {
        decisions.extend(super::client_launch_decision_resources(store, &session)?);
    }
    Ok(Some(json!({
        "version":"st3.visualization.v0", "views":["graph","timeline","swimlane","revision","risk","live-progress"],
        "mission":mission_id, "nodes":nodes, "edges":edges, "groups":[],
        "timeline":{"entries":timeline}, "swimlanes":[], "goals":mission.goals,
        "constraints":mission.constraints, "gates":mission.gates, "resources":mission.products,
        "decisions":decisions, "revision":{"current":revision}, "diffs":[],
        "risk":{"blockers":[],"warnings":[]}, "live_progress":{"state":state}
    })))
}

fn mission_resources(
    store: &Store,
    snapshot_index: u64,
    history: bool,
    selected_id: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    let mut missions = BTreeMap::<String, Vec<MissionRunView>>::new();
    for run in store.mission_run_headers()? {
        missions.entry(run.mission.clone()).or_default().push(run);
    }
    let definitions = store
        .mission_definitions()?
        .into_iter()
        .map(|definition| {
            (
                definition.mission.subject.clone(),
                (definition.mission, definition.updated_at_unix_ms),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for mission in definitions.keys() {
        missions.entry(mission.clone()).or_default();
    }
    let desired = store.desired_subjects()?;
    let mut values = missions
        .into_iter()
        .filter(|(mission, _)| selected_id.is_none_or(|selected| mission == selected))
        .map(|(mission, mut runs)| {
            runs.sort_by_key(|run| run.created_at_unix_ms);
            let definition = definitions.get(&mission);
            let latest = runs.last();
            let state = if runs.is_empty() {
                match definition.map(|(definition, _)| &definition.state) {
                    Some(crate::model::MissionState::Draft) => "draft",
                    Some(crate::model::MissionState::Ready) => "ready",
                    Some(crate::model::MissionState::Retired) => "retired",
                    None => "ready",
                }
            } else if runs.iter().any(|run| run.status == "running") {
                "running"
            } else if runs.iter().any(|run| run.status == "standing") {
                "standing"
            } else {
                match latest
                    .expect("a nonempty run list has a latest run")
                    .status
                    .as_str()
                {
                    "completed" => "completed",
                    "failed" => "failed",
                    "cancelled" => "cancelled",
                    _ => "ready",
                }
            };
            let historical = matches!(state, "completed" | "failed" | "cancelled" | "retired");
            if !history && historical {
                return Ok(None);
            }
            let run_generations = runs
                .iter()
                .map(|run| (run.subject.clone(), Value::String(run.generation.clone())))
                .collect::<serde_json::Map<_, _>>();
            let run_ids = runs
                .iter()
                .map(|run| run.subject.as_str())
                .collect::<BTreeSet<_>>();
            let usage = aggregate_usage_for_runs(store, &desired, &run_ids, Some(snapshot_index))?;
            let revision = latest
                .map(|run| run.revision.as_str())
                .or_else(|| definition.map(|(definition, _)| definition.revision.as_str()))
                .expect("a mission resource has a definition or a run");
            let updated_at_unix_ms = latest
                .map(|run| run.updated_at_unix_ms)
                .or_else(|| definition.map(|(_, updated_at)| *updated_at))
                .expect("a mission resource has a definition or a run timestamp");
            let visualization = if selected_id.is_some() {
                mission_visualization(store, &mission, revision, state)?
            } else {
                None
            };
            Ok::<Option<Value>, anyhow::Error>(Some(json!({
                "id": mission,
                "kind": "mission",
                "revision": revision,
                "updated_at": client_timestamp(updated_at_unix_ms),
                "title": mission.strip_prefix("mission/").unwrap_or(&mission),
                "state": state,
                "mission_revision": revision,
                "runs": runs.into_iter().map(|run| run.subject).collect::<Vec<_>>(),
                "run_generations": run_generations,
                "visualization": visualization,
                "usage": usage,
                "operational": {
                    "layer": if historical { "history" } else { "current" },
                    "actionable": !historical,
                    "reasons": if historical { vec![state] } else { Vec::<&str>::new() }
                }
            })))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right["updated_at"]
            .as_str()
            .cmp(&left["updated_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

fn runtime_resources(
    state: &AppState,
    history: bool,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
) -> anyhow::Result<Vec<Value>> {
    // Runtime authority must come from the same status reduction used by every
    // other control path. A raw claim ordered last by this replica's ingest
    // index is not necessarily the causally current runtime observation.
    let status = state.store.status_for_claim_kind_at(
        "runtime.observed",
        Some(snapshot.store_index),
        history,
    )?;
    let mut values = Vec::new();
    for selected in status.subjects {
        let Some(actual) = selected.actual.as_ref() else {
            continue;
        };
        let fields = actual.get("fields").unwrap_or(actual);
        let Some(runtime_id) = fields.get("runtime_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(actual_claim) = selected.actual_claim.as_deref() else {
            continue;
        };
        let Some(actual_origin) = selected.actual_origin.as_deref() else {
            continue;
        };
        let observed = fields
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let mut runtime_state = match observed {
            "ready" | "working" | "idle" => "running",
            "absent" => "stopped",
            other @ ("pending" | "starting" | "running" | "stopping" | "stopped" | "exited"
            | "failed" | "unreachable") => other,
            _ => "pending",
        };
        let terminal = fields
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owner_id = selected.subject.clone();
        let terminal_id = terminal.then(|| format!("terminal/{owner_id}"));
        let owner_host_id = client_host_id(actual_origin);
        let authoritative = matches!(selected.reachability.as_str(), "reachable" | "local");
        if !authoritative {
            runtime_state = "unreachable";
        }
        let local = authoritative && observed == "running" && actual_origin == state.store.origin();
        let incarnation_id = fields.get("incarnation_id").and_then(Value::as_str);
        let updated_at = state
            .store
            .claim_by_id(actual_claim)?
            .map(|claim| client_timestamp(claim.accepted_at_unix_ms))
            .unwrap_or_else(|| snapshot.created_at.clone());
        let reasons = selected.reason.into_iter().collect::<Vec<_>>();
        values.push(json!({
            "id": format!("runtime/{runtime_id}"),
            "kind": "runtime",
            "revision": actual_claim,
            "updated_at": updated_at,
            "runtime_kind": if terminal { "terminal" } else { "agent" },
            "owner_id": owner_id,
            "owner_host_id": owner_host_id,
            "state": runtime_state,
            "runtime_id": runtime_id,
            "incarnation_id": incarnation_id,
            "desired_revision": actual_claim,
            "owner_run_id": selected.owner_run,
            "terminal_id": terminal_id,
            "terminal_sequence": terminal.then_some(snapshot.store_index),
            "terminal_access": terminal.then(|| json!({
                "read": if local && session.allows("terminal.read") { "granted" } else { "unavailable" },
                "input": if local && session.allows("terminal.control") { "granted" } else { "unavailable" },
                "resize": if local && session.allows("terminal.control") { "granted" } else { "unavailable" }
            })),
            "operational": {
                "layer": selected.projection.layer,
                "actionable": authoritative,
                "reasons": reasons,
                "runtime_incarnation": incarnation_id
            }
        }));
    }
    values.sort_by(|left, right| {
        left["owner_id"]
            .as_str()
            .cmp(&right["owner_id"].as_str())
            .then_with(|| {
                left["runtime_kind"]
                    .as_str()
                    .cmp(&right["runtime_kind"].as_str())
            })
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

fn machine_resources(
    state: &AppState,
    history: bool,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
) -> anyhow::Result<Vec<Value>> {
    let runtimes = runtime_resources(state, history, snapshot, session)?;
    let mut host_runtime_ids = BTreeMap::<String, BTreeSet<String>>::new();
    let mut host_running_runtimes = BTreeMap::<String, usize>::new();
    let mut host_current_runtime_membership = BTreeSet::<String>::new();
    let mut runtime_owner_hosts = BTreeMap::<String, String>::new();
    let mut host_updated_at = BTreeMap::<String, String>::new();
    for runtime in &runtimes {
        let Some(host_id) = runtime["owner_host_id"].as_str() else {
            continue;
        };
        let Some(runtime_id) = runtime["id"].as_str() else {
            continue;
        };
        host_runtime_ids
            .entry(host_id.to_owned())
            .or_default()
            .insert(runtime_id.to_owned());
        if runtime["state"].as_str() == Some("running") {
            *host_running_runtimes.entry(host_id.to_owned()).or_default() += 1;
        }
        if runtime
            .pointer("/operational/layer")
            .and_then(Value::as_str)
            == Some("current")
        {
            host_current_runtime_membership.insert(host_id.to_owned());
        }
        if let Some(updated_at) = runtime["updated_at"].as_str() {
            host_updated_at
                .entry(host_id.to_owned())
                .and_modify(|current| {
                    if updated_at > current.as_str() {
                        *current = updated_at.to_owned();
                    }
                })
                .or_insert_with(|| updated_at.to_owned());
        }
        if let Some(owner_id) = runtime["owner_id"].as_str() {
            runtime_owner_hosts.insert(owner_id.to_owned(), host_id.to_owned());
        }
    }

    let work = super::client_work_resources(
        &state.store,
        None,
        history,
        client_snapshot_time(snapshot),
        snapshot.store_index,
    )?;
    let mut host_work = BTreeMap::<String, BTreeSet<String>>::new();
    for item in work {
        let Some(claimant) = item["claimant"].as_str() else {
            continue;
        };
        let Some(host_id) = runtime_owner_hosts.get(claimant) else {
            continue;
        };
        if let Some(work_id) = item["id"].as_str() {
            host_work
                .entry(host_id.clone())
                .or_default()
                .insert(work_id.to_owned());
        }
        if let Some(updated_at) = item["updated_at"].as_str() {
            host_updated_at
                .entry(host_id.clone())
                .and_modify(|current| {
                    if updated_at > current.as_str() {
                        *current = updated_at.to_owned();
                    }
                })
                .or_insert_with(|| updated_at.to_owned());
        }
    }

    let local_host = client_host_id(&state.node);
    let mut host_ids = BTreeSet::from([local_host.clone()]);
    let configured_hosts = state
        .configured_peers
        .iter()
        .map(|peer| client_host_id(peer))
        .collect::<BTreeSet<_>>();
    host_ids.extend(configured_hosts.iter().cloned());
    host_ids.extend(host_runtime_ids.keys().cloned());
    let status =
        state
            .store
            .status_for_subject_prefix_at("host/", Some(snapshot.store_index), history)?;
    let mut host_statuses = status
        .subjects
        .into_iter()
        .filter(|subject| {
            subject.kind.as_deref() == Some("host") || subject.subject.starts_with("host/")
        })
        .map(|subject| (subject.subject.clone(), subject))
        .collect::<BTreeMap<_, _>>();
    if history {
        host_ids.extend(host_statuses.keys().cloned());
    }

    let mut machines = Vec::new();
    for host_id in host_ids {
        let name = host_id.strip_prefix("host/").unwrap_or(&host_id).to_owned();
        let current_member = host_id == local_host
            || configured_hosts.contains(&host_id)
            || host_current_runtime_membership.contains(&host_id);
        let mut updated_at = host_updated_at
            .get(&host_id)
            .cloned()
            .unwrap_or_else(|| client_timestamp(0));
        let (
            machine_state,
            transports,
            operational_layer,
            operational_actionable,
            operational_reasons,
        ) = if host_id == local_host {
            (
                "local",
                vec![json!({
                    "protocol": "unix",
                    "status": "local",
                    "last_success_at": Value::Null,
                })],
                "current".to_owned(),
                true,
                vec!["authoritative-local-host".to_owned()],
            )
        } else if let Some(selected) = host_statuses.remove(&host_id) {
            let actual = selected.actual.as_ref();
            let fields = actual
                .and_then(|actual| actual.get("fields"))
                .or(actual)
                .unwrap_or(&Value::Null);
            let status = fields
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let conflicted = !selected.conflicts.is_empty()
                || !matches!(selected.reachability.as_str(), "reachable" | "local");
            let machine_state = match (conflicted, status) {
                (true, _) | (false, "unknown") => "indeterminate",
                (false, "up") => "reachable",
                (false, "down") => "unreachable",
                (false, _) => "indeterminate",
            };
            if let Some(claim) = selected.actual_claim.as_deref()
                && let Some(claim) = state.store.claim_by_id(claim)?
            {
                let claim_updated_at = client_timestamp(claim.accepted_at_unix_ms);
                if claim_updated_at > updated_at {
                    updated_at = claim_updated_at;
                }
            }
            let layer = if current_member {
                selected.projection.layer.clone()
            } else {
                "history".to_owned()
            };
            let mut reasons = selected.projection.reasons.clone();
            if current_member {
                reasons.push("replication-transport".to_owned());
            } else {
                reasons.push("discovered-history".to_owned());
            }
            if conflicted {
                reasons.push("authority-indeterminate".to_owned());
            }
            reasons.sort();
            reasons.dedup();
            (
                machine_state,
                vec![json!({
                    "protocol": fields.get("protocol").and_then(Value::as_str).unwrap_or("replication"),
                    "status": if matches!(status, "up" | "down") { status } else { "unknown" },
                    "last_success_at": fields.get("last_success_at").and_then(Value::as_u64).map(|value| client_timestamp(u128::from(value))),
                })],
                layer.clone(),
                layer == "current"
                    && selected.projection.actionable
                    && machine_state != "indeterminate",
                reasons,
            )
        } else {
            let reason = if configured_hosts.contains(&host_id) {
                "configured-unobserved"
            } else {
                "runtime-owner-host-unobserved"
            };
            (
                "indeterminate",
                vec![json!({
                    "protocol": "replication",
                    "status": "unknown",
                    "last_success_at": Value::Null,
                })],
                "current".to_owned(),
                false,
                vec![reason.to_owned()],
            )
        };
        let running_runtimes = host_running_runtimes.remove(&host_id).unwrap_or_default();
        let runtime_ids = host_runtime_ids
            .remove(&host_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let work = host_work
            .remove(&host_id)
            .unwrap_or_default()
            .into_iter()
            .collect::<Vec<_>>();
        let mut machine = json!({
            "host_id": host_id,
            "name": name.clone(),
            "state": machine_state,
            "fleet_id": state.fleet_id,
            "capacity": {
                "state": "unknown",
                "reason": "no capacity observation",
            },
            "occupancy": {
                "running_runtimes": running_runtimes,
            },
            "projects": [],
            "work": work,
            "transports": transports,
            "runtime_ids": runtime_ids,
            "operational": {
                "layer": operational_layer,
                "actionable": operational_actionable,
                "reasons": operational_reasons,
            }
        });
        let revision = format!(
            "machine:{}",
            hex::encode(Sha256::digest(
                serde_json::to_vec(&machine).expect("machine projection serializes")
            ))
        );
        let fields = machine
            .as_object_mut()
            .expect("machine projection is an object");
        fields.insert("id".into(), Value::String(format!("machine/{name}")));
        fields.insert("kind".into(), Value::String("machine".into()));
        fields.insert("revision".into(), Value::String(revision));
        fields.insert("updated_at".into(), Value::String(updated_at));
        machines.push(machine);
    }
    machines.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(machines)
}

fn operation_resources(state: &AppState, at: &str) -> Result<Vec<Value>, ApiError> {
    let report = doctor_report(state)?.0;
    let mut values = report
        .checks
        .into_iter()
        .map(|check| {
            let digest = hex::encode(Sha256::digest(check.name.as_bytes()));
            json!({
                "id": format!("operation/diagnostic-{}", &digest[..16]),
                "kind": "operation",
                "revision": format!("{}:{}", check.name, check.status),
                "updated_at": at,
                "component": match check.name.as_str() { "replication" => "transport", "runtime-drift" | "runtime-ownership" | "pty-runtime" | "driver-readiness" => "runtime", _ => "daemon" },
                "severity": match check.status.as_str() { "fail" => "critical", "warn" => "warning", _ => "info" },
                "state": match check.status.as_str() { "fail" => "failed", "warn" => "degraded", _ => "healthy" },
                "summary": check.message,
                "targets": [],
                "operational": { "layer": "current", "actionable": false, "reasons": ["diagnostic"] }
            })
        })
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        let severity = |value: &Value| match value["severity"].as_str() {
            Some("critical") => 0,
            Some("warning") => 1,
            _ => 2,
        };
        severity(left)
            .cmp(&severity(right))
            .then_with(|| left["component"].as_str().cmp(&right["component"].as_str()))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(values)
}

pub(super) async fn missions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    if query.cursor.is_some() {
        return client_page(&state, &snapshot, "missions", Vec::new(), &query).map(Json);
    }
    let store = state.store.clone();
    let snapshot_index = snapshot.store_index;
    let history = query.history;
    let items =
        super::blocking_store(move || mission_resources(&store, snapshot_index, history, None))
            .await?;
    client_page(&state, &snapshot, "missions", items, &query).map(Json)
}

pub(super) async fn mission_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let store = state.store.clone();
    let snapshot_index = snapshot.store_index;
    let selected = client_detail_id("mission", &id);
    let items = super::blocking_store(move || {
        mission_resources(&store, snapshot_index, true, Some(&selected))
    })
    .await?;
    client_detail(items, "mission", &id)
}

pub(super) async fn runtimes(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    if query.cursor.is_some() {
        return client_page(&state, &snapshot, "runtimes", Vec::new(), &query).map(Json);
    }
    let items = runtime_resources(&state, query.history, &snapshot, &session)
        .map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "runtimes", items, &query).map(Json)
}

pub(super) async fn terminals(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    if query.cursor.is_some() {
        return client_page(&state, &snapshot, "terminals", Vec::new(), &query).map(Json);
    }
    let mut items = runtime_resources(&state, query.history, &snapshot, &session)
        .map_err(ApiError::internal)?;
    items.retain(|item| item.get("terminal_id").is_some_and(Value::is_string));
    client_page(&state, &snapshot, "terminals", items, &query).map(Json)
}

pub(super) async fn runtime_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        runtime_resources(&state, true, &snapshot, &session).map_err(ApiError::internal)?,
        "runtime",
        &id,
    )
}

pub(super) async fn operations(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let items = operation_resources(&state, &snapshot.created_at)?;
    client_page(&state, &snapshot, "operations", items, &query).map(Json)
}

pub(super) async fn now(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let person = person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let mut items =
        super::client_attention_resources(&state.store, person.as_deref(), query.history)
            .map_err(ApiError::internal)?;
    // The default Now view is the person's attention queue. Mission work belongs
    // in Control; only an explicit work filter opts it into this combined view.
    if query.actor.is_some() || query.owner_run.is_some() {
        let mut work = super::client_work_resources(
            &state.store,
            query.actor.as_deref(),
            query.history,
            client_snapshot_time(&snapshot),
            snapshot.store_index,
        )
        .map_err(ApiError::internal)?;
        if let Some(owner_run) = query.owner_run.as_deref() {
            work.retain(|item| item["mission_run_id"].as_str() == Some(owner_run));
        }
        items.extend(work);
    }
    let priority = |item: &Value| match item["kind"].as_str() {
        Some("attention") => 0,
        Some("work") => 1,
        _ => 2,
    };
    items.sort_by(|left, right| {
        priority(left)
            .cmp(&priority(right))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    client_page(&state, &snapshot, "now", items, &effective_query).map(Json)
}

pub(super) async fn machines(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let items = machine_resources(&state, query.history, &snapshot, &session)
        .map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "machines", items, &query).map(Json)
}

fn device_resources(
    state: &AppState,
    snapshot: &ClientSnapshot,
    person: &str,
) -> Result<Vec<Value>, ApiError> {
    let before = snapshot.store_index.checked_add(1);
    let mut claims = state
        .store
        .claims_page(None, None, 0, before, false, 100_000)
        .map_err(ApiError::internal)?
        .claims;
    claims.sort_by_key(|claim| claim.store_index);
    let mut paired = BTreeMap::<String, (&ClaimRecord, Option<&ClaimRecord>)>::new();
    let mut names = BTreeMap::<String, String>::new();
    for claim in &claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        if claim.kind == "custom.client.pairing-begun"
            && let Some(name) = fields.get("device_name").and_then(Value::as_str)
        {
            names.insert(claim.subject.clone(), name.to_owned());
        }
        let Some(device_id) = fields.get("device_id").and_then(Value::as_str) else {
            continue;
        };
        match claim.kind.as_str() {
            "custom.client.pairing-completed"
                if fields.get("person_id").and_then(Value::as_str) == Some(person) =>
            {
                paired.insert(device_id.to_owned(), (claim, None));
            }
            "custom.client.pairing-revoked" => {
                if let Some(entry) = paired.get_mut(device_id) {
                    entry.1 = Some(claim);
                }
            }
            _ => {}
        }
    }
    let at = client_snapshot_time(snapshot);
    let mut resources = paired
        .into_iter()
        .map(|(device_id, (completed, revoked))| {
            let fields = completed.body.get("fields").unwrap_or(&completed.body);
            let expires_at = fields
                .get("expires_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or_default();
            let selected = revoked.unwrap_or(completed);
            let state_name = if revoked.is_some() {
                "revoked"
            } else if expires_at <= at {
                "expired"
            } else {
                "active"
            };
            json!({
                "id": device_id,
                "kind": "device",
                "revision": selected.id,
                "updated_at": client_timestamp(selected.accepted_at_unix_ms),
                "person_id": person,
                "name": names.get(&completed.subject),
                "session_actor": fields.get("session_actor").cloned().unwrap_or(Value::Null),
                "state": state_name,
                "scopes": fields.get("scopes").cloned().unwrap_or_else(|| json!([])),
                "expires_at": client_timestamp(expires_at),
                "operational": {
                    "layer": if state_name == "active" { "current" } else { "history" },
                    "actionable": state_name == "active",
                    "reasons": if state_name == "active" { Vec::<&str>::new() } else { vec![state_name] }
                }
            })
        })
        .collect::<Vec<_>>();
    resources.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    Ok(resources)
}

pub(super) async fn devices(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    require_scope(&session, "read.projections")?;
    let person = person_filter(&session, query.person.as_deref())?
        .filter(|person| person.starts_with("person/"))
        .ok_or_else(|| forbidden("device inventory requires an explicitly authenticated person"))?;
    let mut effective_query = query.clone();
    effective_query.person = Some(person.clone());
    if effective_query.cursor.is_some() {
        return client_page(&state, &snapshot, "devices", Vec::new(), &effective_query).map(Json);
    }
    let mut items = device_resources(&state, &snapshot, &person)?;
    if !query.history {
        items.retain(|item| item["state"] == "active");
    }
    client_page(&state, &snapshot, "devices", items, &effective_query).map(Json)
}

pub(super) async fn operation_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    client_detail(
        operation_resources(&state, &snapshot.created_at)?,
        "operation",
        &id,
    )
}

fn timeline_attribution(owner: &str, desired: &[crate::model::DesiredSubject]) -> Value {
    let ownership = desired.iter().find(|desired| desired.subject == owner);
    json!({
        "agent_id": owner,
        "mission_run_id": ownership.and_then(|value| value.owner_run.as_deref()),
        "generation_id": ownership.and_then(|value| value.owner_generation.as_deref()),
        "step_id": ownership.and_then(|value| value.owner_step.as_deref()),
    })
}

fn timeline_retention_is_explicit(claims: &[ClaimRecord], has_older: bool) -> bool {
    if !has_older {
        return true;
    }
    let earliest_retained = claims
        .iter()
        .filter_map(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            (fields.get("entry_type").and_then(Value::as_str) != Some("truncation"))
                .then(|| fields.get("sequence").and_then(Value::as_u64))
                .flatten()
        })
        .min();
    let Some(required_through) = earliest_retained.and_then(|sequence| sequence.checked_sub(1))
    else {
        return false;
    };
    let mut intervals = claims
        .iter()
        .filter_map(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            (fields.get("operation").and_then(Value::as_str) == Some("append")
                && fields.get("entry_type").and_then(Value::as_str) == Some("truncation"))
            .then(|| {
                fields
                    .pointer("/body/omitted_from_sequence")
                    .and_then(Value::as_u64)
                    .zip(
                        fields
                            .pointer("/body/omitted_to_sequence")
                            .and_then(Value::as_u64),
                    )
            })
            .flatten()
            .filter(|(from, to)| from <= to)
        })
        .collect::<Vec<_>>();
    intervals.sort_unstable();
    let mut covered_through = 0_u64;
    for (from, to) in intervals {
        if from > covered_through.saturating_add(1) {
            break;
        }
        covered_through = covered_through.max(to);
        if covered_through >= required_through {
            return true;
        }
    }
    false
}

fn normalized_timeline_usage_body(
    body: Value,
    attribution: &Value,
    source_driver: Option<&str>,
) -> Value {
    let mut body = body.as_object().cloned().unwrap_or_default();
    if !matches!(
        body.get("semantics").and_then(Value::as_str),
        Some("context_occupancy" | "session_cumulative" | "response")
    ) {
        // Legacy/provider events without declared cumulative semantics are one
        // response observation; treating them as cumulative would undercount.
        body.insert("semantics".into(), Value::String("response".into()));
    }
    if let Some(driver) = source_driver.filter(|driver| !driver.is_empty()) {
        body.insert("driver".into(), Value::String(driver.into()));
    } else if !body
        .get("driver")
        .and_then(Value::as_str)
        .is_some_and(|driver| !driver.is_empty())
    {
        body.insert("driver".into(), Value::String("unknown".into()));
    }
    body.insert("attribution".into(), attribution.clone());
    Value::Object(body)
}

fn native_timeline_page(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session_id: &str,
    query: &ClientListQuery,
    external: &crate::external_sessions::ExternalSession,
) -> Result<Json<Value>, ApiError> {
    let mut items =
        crate::external_sessions::normalized_timeline(external).map_err(ApiError::internal)?;
    items.reverse();
    let mut page = client_page(
        state,
        snapshot,
        &format!("timeline/{session_id}"),
        items,
        query,
    )?;
    page.items.reverse();
    Ok(Json(json!({
        "kind": "timeline-page",
        "session_id": session_id,
        "items": page.items,
        "page": page.page
    })))
}

fn managed_codex_transcript(
    state: &AppState,
    owner: &str,
    incarnation: &str,
) -> Result<Option<crate::external_sessions::ExternalSession>, ApiError> {
    let Some(home) = state.native_session_home.as_deref() else {
        return Ok(None);
    };
    // The wrapper owns this path; never resolve a path from client input. A reused
    // driver directory is only authoritative when its runtime and a durable
    // observation both name the same exact provider incarnation.
    let directory = state
        .state_dir
        .join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
        .join("state");
    let Ok(runtime) = std::fs::read(directory.join("runtime.json")) else {
        return Ok(None);
    };
    let Ok(binding) = std::fs::read(directory.join("binding.json")) else {
        return Ok(None);
    };
    let (Ok(runtime), Ok(binding)) = (
        serde_json::from_slice::<Value>(&runtime),
        serde_json::from_slice::<Value>(&binding),
    ) else {
        return Ok(None);
    };
    let identity = owner.strip_prefix("agent/").unwrap_or(owner);
    let Some(provider_incarnation) = runtime["incarnation"].as_str() else {
        return Ok(None);
    };
    let Some(native_id) = binding["threadId"].as_str() else {
        return Ok(None);
    };
    if runtime["agent"] != identity
        || binding["agent"] != identity
        || binding["runtimeIncarnation"] != provider_incarnation
    {
        return Ok(None);
    }
    let observed = state
        .store
        .latest_claim(owner, Some("harness.observed"))
        .map_err(ApiError::internal)?
        .is_some_and(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            fields["driver"] == "codex"
                && fields["incarnation_id"] == incarnation
                && fields["evidence_incarnation"] == provider_incarnation
        });
    if !observed {
        return Ok(None);
    }
    Ok(crate::external_sessions::discover(Some(home), true)
        .map_err(ApiError::internal)?
        .sessions
        .into_iter()
        .find(|session| {
            session.driver == crate::external_sessions::ExternalDriver::Codex
                && session.native_id == native_id
        }))
}

fn managed_claude_transcript(
    state: &AppState,
    owner: &str,
    incarnation: &str,
) -> Result<Option<crate::external_sessions::ExternalSession>, ApiError> {
    let Some(home) = state.native_session_home.as_deref() else {
        return Ok(None);
    };
    let identity = owner.strip_prefix("agent/").unwrap_or(owner);
    let directory = state
        .state_dir
        .join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
        .join("catalog")
        .join("agents")
        .join(st2::run::detect_host())
        .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16]);
    // Only the current wrapper's SessionStart hook may bind a Claude transcript.
    // A previous provider's session-id file can survive a restart, so neither
    // its presence nor the newest transcript in a workspace is sufficient.
    let Ok(binding) = std::fs::read(directory.join("claude-native-session")) else {
        return Ok(None);
    };
    let Ok(binding) = serde_json::from_slice::<Value>(&binding) else {
        return Ok(None);
    };
    let (Some(provider_incarnation), Some(native_id)) = (
        binding["incarnation"].as_str(),
        binding["native_session_id"].as_str(),
    ) else {
        return Ok(None);
    };
    let observed = state
        .store
        .latest_claim(owner, Some("harness.observed"))
        .map_err(ApiError::internal)?
        .is_some_and(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            fields["driver"] == "claude"
                && fields["incarnation_id"] == incarnation
                && fields["evidence_incarnation"] == provider_incarnation
        });
    if !observed {
        return Ok(None);
    }
    crate::external_sessions::find_bound_transcript(
        home,
        crate::external_sessions::ExternalDriver::Claude,
        native_id,
    )
    .map_err(ApiError::internal)
}

fn managed_omp_transcript(
    state: &AppState,
    owner: &str,
    incarnation: &str,
) -> Result<Option<crate::external_sessions::ExternalSession>, ApiError> {
    let Some((_, started_at)) = incarnation.split_once(':') else {
        return Ok(None);
    };
    let Ok(started_at) = chrono::DateTime::parse_from_rfc3339(started_at) else {
        return Ok(None);
    };
    let observed = state
        .store
        .latest_claim(owner, Some("harness.observed"))
        .map_err(ApiError::internal)?
        .is_some_and(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            fields["driver"] == "omp" && fields["incarnation_id"] == incarnation
        });
    if !observed {
        return Ok(None);
    }
    let identity = owner.strip_prefix("agent/").unwrap_or(owner);
    let directory = state
        .state_dir
        .join("drivers")
        .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
        .join("catalog")
        .join("agents")
        .join(st2::run::detect_host())
        .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16])
        .join("provider-sessions");
    crate::external_sessions::find_managed_omp_transcript(
        &directory,
        (started_at.timestamp_millis().max(0) as u128).saturating_sub(2_000),
    )
    .map_err(ApiError::internal)
}

pub(super) fn timeline_value(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    id: &str,
    query: &ClientListQuery,
) -> Result<Json<Value>, ApiError> {
    require_scope(session, "read.projections")?;
    let session_id = client_detail_id("session", id);
    if query.cursor.is_some() {
        let mut page = client_page(
            state,
            snapshot,
            &format!("timeline/{session_id}"),
            Vec::new(),
            query,
        )?;
        page.items.reverse();
        return Ok(Json(json!({
            "kind": "timeline-page",
            "session_id": session_id,
            "items": page.items,
            "page": page.page
        })));
    }
    let managed = super::managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
        .map_err(ApiError::internal)?;
    let Some((owner, incarnation, _)) = managed else {
        let external =
            crate::external_sessions::find(state.native_session_home.as_deref(), &session_id)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("session `{session_id}` does not exist"))
                })?;
        return native_timeline_page(state, snapshot, &session_id, query, &external);
    };
    let owner = owner.as_str();
    let incarnation = incarnation.as_deref();
    if let Some(incarnation) = incarnation {
        let external = match managed_codex_transcript(state, owner, incarnation)? {
            Some(external) => Some(external),
            None => match managed_claude_transcript(state, owner, incarnation)? {
                Some(external) => Some(external),
                None => managed_omp_transcript(state, owner, incarnation)?,
            },
        };
        if let Some(external) = external {
            return native_timeline_page(state, snapshot, &session_id, query, &external);
        }
    }
    let desired = state.store.desired_subjects().map_err(ApiError::internal)?;
    let attribution = timeline_attribution(owner, &desired);
    let before = snapshot.store_index.checked_add(1);
    let timeline_page = if let Some(incarnation) = incarnation {
        state
            .store
            .timeline_claims_for_incarnation_at(owner, incarnation, before, true, 4_096)
            .map_err(ApiError::internal)?
    } else {
        crate::model::ClaimsPage {
            claims: Vec::new(),
            next_cursor: None,
        }
    };
    let has_older_timeline = timeline_page.next_cursor.is_some();
    let mut timeline_claims = timeline_page.claims;
    timeline_claims.reverse();
    timeline_claims.retain(|claim| {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        incarnation.is_some_and(|expected| {
            fields.get("incarnation_id").and_then(Value::as_str) == Some(expected)
        })
    });
    if !timeline_retention_is_explicit(&timeline_claims, has_older_timeline) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "older timeline history was omitted without a typed truncation interval"
                .into(),
            details: Box::new(serde_json::Map::from_iter([(
                "full_resync".into(),
                Value::Bool(true),
            )])),
        });
    }
    let mut retained_entries = BTreeSet::new();
    for claim in &timeline_claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let Some(entry_id) = fields.get("entry_id").and_then(Value::as_str) else {
            continue;
        };
        let operation = fields.get("operation").and_then(Value::as_str);
        if !retained_entries.contains(entry_id) && operation != Some("append") {
            return Err(ApiError {
                status: StatusCode::GONE,
                code: "cursor-gap".into(),
                message: "the retained timeline begins after an entry's append operation".into(),
                details: Box::new(serde_json::Map::from_iter([(
                    "full_resync".into(),
                    Value::Bool(true),
                )])),
            });
        }
        retained_entries.insert(entry_id.to_owned());
    }
    let mut owner_claims = state
        .store
        .claims_page(Some(owner), None, 0, before, true, 10_000)
        .map_err(ApiError::internal)?
        .claims;
    owner_claims.reverse();
    owner_claims.retain(|claim| claim.kind != "harness.timeline");
    let mut message_claims = state
        .store
        .claims_for_kind_at("message.sent", before, true, 10_000)
        .map_err(ApiError::internal)?
        .claims;
    message_claims.reverse();
    let mut claims = timeline_claims;
    claims.extend(owner_claims);
    claims.extend(message_claims);
    claims.sort_by_key(|claim| claim.store_index);
    claims.dedup_by_key(|claim| claim.id.clone());
    let session_leaf = session_id.trim_start_matches("session/");
    let mut items = Vec::<Value>::new();
    let mut explicit = BTreeMap::<String, usize>::new();
    let mut tool_calls = BTreeSet::<String>::new();
    let entry_id = |claim: &ClaimRecord, suffix: &str| {
        let digest = hex::encode(Sha256::digest(format!("{}:{suffix}", claim.id).as_bytes()));
        format!("timeline-entry/{session_leaf}/{}", &digest[..24])
    };
    let applies_to_incarnation = |fields: &Value| {
        let observed = fields.get("incarnation_id").and_then(Value::as_str);
        observed.is_none() || incarnation.is_none() || observed == incarnation
    };
    for claim in claims {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        let timestamp = client_timestamp(
            fields
                .get("observed_at_unix_ms")
                .and_then(Value::as_u64)
                .map(u128::from)
                .unwrap_or(claim.accepted_at_unix_ms),
        );
        let base_sequence = claim.store_index.saturating_mul(4);
        if claim.subject == owner && claim.kind == "harness.timeline" {
            if !incarnation.is_some_and(|expected| {
                fields.get("incarnation_id").and_then(Value::as_str) == Some(expected)
            }) {
                continue;
            }
            let Some(operation) = fields.get("operation").and_then(Value::as_str) else {
                continue;
            };
            let Some(id) = fields.get("entry_id").and_then(Value::as_str) else {
                continue;
            };
            let revision = fields.get("revision").and_then(Value::as_u64).unwrap_or(1);
            let role = fields
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("system");
            let entry_type = fields
                .get("entry_type")
                .and_then(Value::as_str)
                .unwrap_or("error");
            let final_entry = fields
                .get("final")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let sequence = fields
                .get("sequence")
                .and_then(Value::as_u64)
                .unwrap_or(base_sequence + 3);
            let mut body = fields.get("body").cloned().unwrap_or_else(|| json!({}));
            if entry_type == "usage" {
                body = normalized_timeline_usage_body(
                    body,
                    &attribution,
                    fields.get("driver").and_then(Value::as_str),
                );
            }
            let transition_valid = match operation {
                "append" => !explicit.contains_key(id) && revision == 1,
                "replace" | "finalize" => explicit.get(id).is_some_and(|index| {
                    let current = &items[*index];
                    !current["final"].as_bool().unwrap_or(false)
                        && current["revision"].as_u64().unwrap_or(0) + 1 == revision
                        && current["role"] == role
                        && current["type"] == entry_type
                }),
                _ => false,
            };
            let tool_order_valid = if entry_type == "tool_result" {
                body.get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|call_id| tool_calls.contains(call_id))
            } else {
                true
            };
            if !transition_valid || !tool_order_valid {
                items.push(json!({
                    "id": entry_id(&claim, "invalid-transition"),
                    "sequence": sequence,
                    "revision": 1,
                    "timestamp": timestamp,
                    "role": "system",
                    "type": "error",
                    "final": true,
                    "body": {
                        "code": "invalid-timeline-transition",
                        "message": format!("driver timeline entry `{id}` has an invalid {operation} transition"),
                        "retryable": false,
                        "details": { "claim_id": claim.id }
                    }
                }));
                continue;
            }
            if entry_type == "tool_call"
                && let Some(call_id) = body.get("call_id").and_then(Value::as_str)
            {
                tool_calls.insert(call_id.to_owned());
            }
            if operation == "append" {
                explicit.insert(id.to_owned(), items.len());
                items.push(json!({
                    "id": id,
                    "sequence": sequence,
                    "revision": revision,
                    "timestamp": timestamp,
                    "role": role,
                    "type": entry_type,
                    "final": final_entry,
                    "body": body
                }));
            } else if let Some(index) = explicit.get(id).copied() {
                let sequence = items[index]["sequence"].clone();
                let original_timestamp = items[index]["timestamp"].clone();
                items[index] = json!({
                    "id": id,
                    "sequence": sequence,
                    "revision": revision,
                    "timestamp": original_timestamp,
                    "role": role,
                    "type": entry_type,
                    "final": operation == "finalize" || final_entry,
                    "body": body
                });
            }
            continue;
        }
        if claim.subject == owner && claim.kind == "runtime.observed" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            let Some(observed) = fields.get("status").and_then(Value::as_str) else {
                continue;
            };
            let status = match observed {
                "pending" | "starting" => "queued",
                "running" | "ready" | "working" => "running",
                "idle" | "blocked" => "waiting",
                "stopped" | "exited" | "absent" => "completed",
                "failed" => "failed",
                "cancelled" => "cancelled",
                _ => continue,
            };
            items.push(json!({
                "id": entry_id(&claim, "runtime-status"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "status",
                "final": true, "body": { "status": status, "detail": observed }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.observed" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            let observed = fields
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("indeterminate");
            let status = match observed {
                "starting" => "queued",
                "ready" | "working" => "running",
                "idle" | "blocked" => "waiting",
                "ended" => "completed",
                _ => "failed",
            };
            items.push(json!({
                "id": entry_id(&claim, "harness-status"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "status",
                "final": true, "body": { "status": status, "detail": observed }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.diagnostic" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            items.push(json!({
                "id": entry_id(&claim, "diagnostic"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "error",
                "final": true, "body": {
                    "code": fields.get("code").and_then(Value::as_str).unwrap_or("harness-diagnostic"),
                    "message": fields.get("reason").and_then(Value::as_str).unwrap_or("the harness reported a diagnostic"),
                    "retryable": fields.get("severity").and_then(Value::as_str) == Some("warning"),
                    "details": { "severity": fields.get("severity"), "claim_id": claim.id }
                }
            }));
            continue;
        }
        if claim.subject == owner && claim.kind == "harness.usage" {
            if !applies_to_incarnation(fields) {
                continue;
            }
            let mut body = fields.as_object().cloned().unwrap_or_default();
            body.remove("incarnation_id");
            let body = normalized_timeline_usage_body(
                Value::Object(body),
                &attribution,
                fields.get("driver").and_then(Value::as_str),
            );
            items.push(json!({
                "id": entry_id(&claim, "usage"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": "system", "type": "usage",
                "final": true, "body": body
            }));
            continue;
        }
        if claim.kind == "message.sent" {
            if fields.get("session_id").and_then(Value::as_str) != Some(session_id.as_str()) {
                continue;
            }
            let from = fields.get("from").and_then(Value::as_str);
            let to = fields.get("to").and_then(Value::as_str);
            if from != Some(owner) && to != Some(owner) {
                continue;
            }
            let role = if from == Some(owner) {
                "assistant"
            } else {
                "user"
            };
            items.push(json!({
                "id": entry_id(&claim, "message"), "sequence": base_sequence,
                "revision": 1, "timestamp": timestamp, "role": role, "type": "message",
                "final": true, "body": {
                    "message_id": claim.subject,
                    "reply_to": fields.get("in_reply_to")
                }
            }));
            items.push(json!({
                "id": entry_id(&claim, "content"), "sequence": base_sequence + 1,
                "revision": 1, "timestamp": timestamp, "role": role, "type": "content",
                "final": true, "body": {
                    "media_type": "text/plain",
                    "text": fields.get("content").and_then(Value::as_str).unwrap_or_default()
                }
            }));
        }
    }
    items.sort_by_key(|item| item["sequence"].as_u64().unwrap_or(u64::MAX));
    // A conversation opens at its newest bounded window. The cursor walks toward older
    // windows, while each individual page remains chronological for straightforward rendering.
    items.reverse();
    let mut page = client_page(
        state,
        snapshot,
        &format!("timeline/{session_id}"),
        items,
        query,
    )?;
    page.items.reverse();
    Ok(Json(json!({
        "kind": "timeline-page",
        "session_id": session_id,
        "items": page.items,
        "page": page.page
    })))
}

#[derive(Default, Deserialize)]
pub(super) struct EventsQuery {
    after: Option<String>,
    limit: Option<usize>,
    wait_ms: Option<u64>,
}

fn decode_event_cursor(node: &str, cursor: Option<&str>) -> Result<u64, ApiError> {
    let Some(cursor) = cursor else { return Ok(0) };
    let mut parts = cursor.split('/');
    if parts.next() != Some("event-cursor") || parts.next() != Some(node) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the event cursor belongs to another host or retention epoch".into(),
            details: Box::new(serde_json::Map::from_iter([(
                "full_resync".into(),
                Value::Bool(true),
            )])),
        });
    }
    parts
        .next()
        .and_then(|value| value.parse().ok())
        .filter(|_| parts.next().is_none())
        .ok_or_else(|| validation("the event cursor is malformed"))
}

fn event_resume_floor(oldest: u64) -> u64 {
    oldest.saturating_sub(1)
}

fn validate_event_cursor(
    node: &str,
    cursor_was_supplied: bool,
    after: u64,
    oldest: u64,
    newest: u64,
) -> Result<(), ApiError> {
    let floor = event_resume_floor(oldest);
    if cursor_was_supplied && (after < floor || after > newest) {
        return Err(ApiError {
            status: StatusCode::GONE,
            code: "cursor-gap".into(),
            message: "the event cursor is outside the retained event range".into(),
            details: Box::new(serde_json::Map::from_iter([
                ("full_resync".into(), Value::Bool(true)),
                (
                    "oldest_cursor".into(),
                    Value::String(format!("event-cursor/{node}/{floor}")),
                ),
                (
                    "newest_cursor".into(),
                    Value::String(format!("event-cursor/{node}/{newest}")),
                ),
            ])),
        });
    }
    Ok(())
}

fn client_session_id(owner: &str, incarnation: &str) -> String {
    let digest = hex::encode(Sha256::digest(format!("{owner}:{incarnation}").as_bytes()));
    format!("session/{}", &digest[..24])
}

fn safe_event_projection(state: &AppState, record: &EventRecord) -> (String, Vec<String>, Value) {
    let fields = record.body.get("fields").unwrap_or(&record.body);
    if record.kind == "harness.timeline" {
        let resource_ids = fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            .map(|incarnation| vec![client_session_id(&record.subject, incarnation)])
            .unwrap_or_default();
        return (
            "upsert".into(),
            resource_ids,
            json!({ "reason": "session-timeline-invalidated" }),
        );
    }
    if record.kind.starts_with("custom.client.pairing-") {
        return (
            "capabilities.changed".into(),
            Vec::new(),
            json!({ "reason": "authenticated-client-capabilities-changed" }),
        );
    }
    if record.kind.starts_with("custom.client.terminal-") {
        let resource_ids = fields
            .get("terminal_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                state
                    .store
                    .claims_for(&record.subject, Some("custom.client.terminal-attached"))
                    .ok()?
                    .into_iter()
                    .next()?
                    .body
                    .pointer("/fields/terminal_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .map(|terminal| vec![terminal.to_owned()])
            .unwrap_or_default();
        return (
            "upsert".into(),
            resource_ids,
            json!({ "reason": "terminal-viewer-lifecycle-changed" }),
        );
    }
    let mut resource_ids = Vec::new();
    if record.subject.starts_with("message/")
        || record.subject.starts_with("agent/")
        || record.subject.starts_with("step-run/")
        || record.subject.starts_with("mission/")
    {
        resource_ids.push(record.subject.clone());
    } else if record.subject.starts_with("mission-run/")
        && let Ok(Some(mission)) = state.store.mission_for_run(&record.subject)
    {
        resource_ids.push(mission);
    } else if record.subject.starts_with("planning-session/") {
        resource_ids.push(format!(
            "launch/{}",
            record.subject.trim_start_matches("planning-session/")
        ));
    }
    if record.kind == "message.sent"
        && let Some(session_id) = fields.get("session_id").and_then(Value::as_str)
    {
        resource_ids.push(session_id.to_owned());
    }
    let event_type = if record.kind == "runtime.observed"
        && fields.get("status").and_then(Value::as_str) == Some("running")
        && fields.get("terminal").and_then(Value::as_bool) == Some(true)
    {
        "terminal.available"
    } else {
        "upsert"
    };
    (
        event_type.into(),
        resource_ids,
        json!({
            "reason": "client-projection-invalidated",
            "change": record.kind,
            "subject": record.subject,
            "state": fields.get("state").or_else(|| fields.get("status")).cloned()
        }),
    )
}

pub(super) async fn events(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let (oldest, newest) = state.store.event_bounds().map_err(ApiError::internal)?;
    let after = decode_event_cursor(&state.node, query.after.as_deref())?;
    validate_event_cursor(&state.node, query.after.is_some(), after, oldest, newest)?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(query.wait_ms.unwrap_or(0).min(30_000));
    // Subscribe before the first store read. An event between reading an empty
    // page and subscribing must wake this long poll, not wait for another event.
    let mut changed = state.event_notify.subscribe();
    let records = loop {
        let records = if query.after.is_some() {
            state
                .store
                .events_after_bounded(after, limit.saturating_add(1))
        } else {
            state.store.events_tail_bounded(limit)
        }
        .map_err(ApiError::internal)?;
        if !records.is_empty() || tokio::time::Instant::now() >= deadline {
            break records;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !matches!(
            tokio::time::timeout(remaining, changed.changed()).await,
            Ok(Ok(()))
        ) {
            break Vec::new();
        }
    };
    let has_more = query.after.is_some() && records.len() > limit;
    let records = records.into_iter().take(limit).collect::<Vec<_>>();
    let resume = records
        .last()
        .map(|record| record.store_index)
        .unwrap_or(after);
    let items = records
        .into_iter()
        .map(|record| {
            let previous = format!(
                "event-cursor/{}/{}",
                state.node,
                record.store_index.saturating_sub(1)
            );
            let next = format!("event-cursor/{}/{}", state.node, record.store_index);
            let event_snapshot = client_snapshot_at(&state, record.store_index);
            let (event_type, resource_ids, body) = safe_event_projection(&state, &record);
            json!({
                "id": format!("projection-event/{}/{}", state.node, record.store_index),
                "epoch": state.node,
                "sequence": record.store_index,
                "previous_cursor": previous,
                "next_cursor": next,
                "timestamp": event_snapshot.created_at,
                "type": event_type,
                "resource_ids": resource_ids,
                "snapshot_id": event_snapshot.id,
                "body": body
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "kind": "event-page",
        "oldest_cursor": format!("event-cursor/{}/{}", state.node, event_resume_floor(oldest)),
        "resume_cursor": format!("event-cursor/{}/{resume}", state.node),
        "items": items,
        "has_more": has_more
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairingBegin {
    api_version: String,
    device_name: String,
    person_id: String,
    full_control: Option<bool>,
}

pub(super) async fn pairing_begin(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Json(request): Json<PairingBegin>,
) -> Result<Json<Value>, ApiError> {
    if session.transport != "unix" {
        return Err(forbidden("pairing can only begin on the local Unix API"));
    }
    if request.person_id != session.authority_actor {
        return Err(forbidden(
            "the pairing person must match the authenticated Unix person",
        ));
    }
    if request.api_version != CLIENT_API_VERSION
        || request.device_name.trim().is_empty()
        || request.device_name.len() > 120
        || !request.person_id.starts_with("person/")
        || request.person_id.matches('/').count() != 1
    {
        return Err(validation(
            "the pairing request requires a valid version, device name, and concrete initiating person",
        ));
    }
    let mut random = [0_u8; 10];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let code = random[..8]
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect::<String>();
    let stable = hex::encode(Sha256::digest(
        format!("{}:{code}", request.device_name).as_bytes(),
    ));
    let pairing_id = format!("pairing/{}", &stable[..24]);
    let subject = format!("custom/client/pairing-{}", &stable[..24]);
    let expires_at = client_now_ms() + 300_000;
    let person_id = request.person_id;
    let scopes = if request.full_control.unwrap_or(false) {
        ALL_SCOPES
    } else {
        LIMITED_PAIRING_SCOPES
    };
    state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "custom.client.pairing-begun".into(),
            actor: Some(person_id.clone()),
            fields: BTreeMap::from([
                ("pairing_id".into(), Value::String(pairing_id.clone())),
                ("device_name".into(), Value::String(request.device_name)),
                ("person_id".into(), Value::String(person_id)),
                ("scopes".into(), json!(scopes)),
                ("code_hash".into(), Value::String(credential_digest(&code))),
                ("expires_at_unix_ms".into(), json!(expires_at)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(
        json!({ "kind": "pairing-challenge", "pairing_id": pairing_id, "code": code, "expires_at": client_timestamp(expires_at) }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairingComplete {
    api_version: String,
    code: String,
    device_public_key: String,
}

pub(super) async fn pairing_complete(
    State(state): State<AppState>,
    Extension(_session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PairingComplete>,
) -> Result<Json<Value>, ApiError> {
    if request.api_version != CLIENT_API_VERSION || request.device_public_key.len() < 32 {
        return Err(validation(
            "the pairing completion has an invalid version or public key",
        ));
    }
    let pairing_id = client_detail_id("pairing", &id);
    let subject = format!("custom/client/pairing-{id}");
    let begun = state
        .store
        .claims_for_subject_kind_at(&subject, "custom.client.pairing-begun", None, true, 1)
        .map_err(ApiError::internal)?
        .claims
        .into_iter()
        .find(|claim| {
            claim
                .body
                .pointer("/fields/pairing_id")
                .and_then(Value::as_str)
                == Some(pairing_id.as_str())
        })
        .ok_or_else(|| ApiError::not_found(format!("pairing `{pairing_id}` does not exist")))?;
    let used = !state
        .store
        .claims_for_subject_kind_at(&subject, "custom.client.pairing-completed", None, true, 1)
        .map_err(ApiError::internal)?
        .claims
        .is_empty();
    let valid_code = begun
        .body
        .pointer("/fields/code_hash")
        .and_then(Value::as_str)
        == Some(credential_digest(&request.code).as_str());
    let expires = begun
        .body
        .pointer("/fields/expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    if used || !valid_code || expires <= client_now_ms() {
        return Err(forbidden(
            "the pairing code is invalid, expired, or already used",
        ));
    }
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(ApiError::internal)?;
    let credential = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let device_hash = hex::encode(Sha256::digest(request.device_public_key.as_bytes()));
    let device_id = format!("device/{}", &device_hash[..24]);
    let actor_suffix = &device_hash[..16];
    let person_id = begun
        .body
        .pointer("/fields/person_id")
        .and_then(Value::as_str)
        .filter(|actor| actor.starts_with("person/") && actor.matches('/').count() == 1)
        .ok_or_else(|| forbidden("the pairing has no authenticated concrete person"))?
        .to_owned();
    let session_actor = format!("{person_id}/session/{actor_suffix}");
    let expires_at = client_now_ms() + 30 * 24 * 60 * 60 * 1_000;
    // The concrete scope list was sealed into the authenticated local begin
    // claim. Legacy pending challenges retain their original limited grant.
    let scopes = match begun.body.pointer("/fields/scopes") {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().filter(|scope| ALL_SCOPES.contains(scope)))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| validation("the pairing has invalid delegated scopes"))?,
        None => LIMITED_PAIRING_SCOPES.to_vec(),
        Some(_) => return Err(validation("the pairing has invalid delegated scopes")),
    };
    let completed = state.store.append_claim(&ClaimInput {
        subject: begun.subject.clone(),
        kind: "custom.client.pairing-completed".into(),
        actor: Some(person_id.clone()),
        fields: BTreeMap::from([
            ("pairing_id".into(), Value::String(pairing_id)),
            ("device_id".into(), Value::String(device_id.clone())),
            ("session_actor".into(), Value::String(session_actor.clone())),
            ("person_id".into(), Value::String(person_id.clone())),
            ("delegated_by".into(), Value::String(person_id.clone())),
            (
                "credential_hash".into(),
                Value::String(credential_digest(&credential)),
            ),
            (
                "device_public_key".into(),
                Value::String(request.device_public_key),
            ),
            ("scopes".into(), json!(scopes)),
            ("delegated_scopes".into(), json!(scopes)),
            ("expires_at_unix_ms".into(), json!(expires_at)),
        ]),
        evidence: vec![begun.id.clone()],
        expected_subject: Some(Some(begun.id.clone())),
        idempotency_key: None,
    });
    if let Err(error) = completed {
        if error.code == "stale-subject" {
            return Err(forbidden(
                "the pairing code is invalid, expired, or already used",
            ));
        }
        return Err(ApiError::bad(error));
    }
    signal_changed(&state);
    Ok(Json(
        json!({ "kind": "paired-session", "device_id": device_id, "person_id": person_id, "session_actor": session_actor, "credential": credential, "scopes": scopes, "expires_at": client_timestamp(expires_at) }),
    ))
}

fn terminal_subject(id: &str) -> String {
    id.strip_prefix("terminal/").unwrap_or(id).to_owned()
}

fn terminal_live_session(
    state: &AppState,
    subject: &str,
    expected_incarnation: Option<&str>,
) -> Result<LiveSession, ApiError> {
    live_session(state, subject, expected_incarnation).map_err(|error| {
        if error.code == "stale-incarnation" {
            stale(error.message)
        } else {
            error
        }
    })
}

// The gateway issues its own attachment so the capability remains bound to the
// authenticated session here. The owner rechecks the runtime on every signed read.
fn remote_terminal_live_session(
    state: &AppState,
    subject: &str,
    expected_incarnation: &str,
) -> Result<LiveSession, ApiError> {
    let status = state
        .store
        .status(Some(subject))
        .map_err(ApiError::internal)?;
    let selected = status
        .subjects
        .first()
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    if !matches!(selected.reachability.as_str(), "reachable" | "local") {
        return Err(stale("the terminal owner is not reachable"));
    }
    let origin = selected
        .actual_origin
        .as_deref()
        .ok_or_else(|| stale("the terminal owner is unknown"))?;
    if origin == state.store.origin() {
        return terminal_live_session(state, subject, Some(expected_incarnation));
    }
    let owner_host_id = client_host_id(origin);
    if state
        .client_relay
        .as_ref()
        .is_none_or(|relay| !relay.has_peer(&owner_host_id))
    {
        return Err(remote_unavailable(&owner_host_id));
    }
    let actual = selected
        .actual
        .as_ref()
        .ok_or_else(|| ApiError::not_found("the terminal has no runtime"))?;
    let fields = actual.get("fields").unwrap_or(actual);
    if fields.get("status").and_then(Value::as_str) != Some("running") {
        return Err(ApiError::not_found("the terminal is not running"));
    }
    let incarnation = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found("the terminal has no incarnation"))?;
    if incarnation != expected_incarnation {
        return Err(stale("the terminal incarnation fence is stale"));
    }
    let runtime_id = fields
        .get("runtime_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found("the terminal has no runtime ID"))?;
    Ok(LiveSession {
        runtime_id: runtime_id.into(),
        incarnation_id: incarnation.into(),
        owner_host_id,
        terminal: fields
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        driver: None,
    })
}

fn bounded_terminal_line(text: &str) -> (String, bool) {
    if text.len() <= TERMINAL_MAX_LINE_BYTES {
        return (text.to_owned(), false);
    }
    let mut end = TERMINAL_MAX_LINE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

fn terminal_screen_value(
    state: &AppState,
    id: &str,
    expected_incarnation: Option<&str>,
) -> Result<(Value, LiveSession), ApiError> {
    let subject = terminal_subject(id);
    let live = terminal_live_session(state, &subject, expected_incarnation)?;
    if !live.terminal {
        return Err(validation(
            "the requested runtime does not expose a terminal",
        ));
    }
    let screen = st_runtime::PtyRuntime::new(state.pty_root.clone())
        .with_binary(state.pty_binary.to_string_lossy())
        .screen(&live.runtime_id)
        .map_err(ApiError::internal)?;
    let screen_line_count = screen.lines().count();
    let mut lines = screen
        .lines()
        .take(TERMINAL_MAX_LINES)
        .enumerate()
        .map(|(row, text)| {
            let (text, truncated) = bounded_terminal_line(text);
            json!({ "row": row, "text": text, "redacted": false, "truncated": truncated })
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(json!({ "row": 0, "text": "", "redacted": false, "truncated": false }));
    }
    let rows = lines.len().max(1);
    let columns = lines
        .iter()
        .filter_map(|line| line["text"].as_str().map(str::chars).map(Iterator::count))
        .max()
        .unwrap_or(1)
        .max(1);
    let value = json!({
        "kind": "terminal-screen", "terminal_id": client_detail_id("terminal", id),
        "runtime_incarnation": live.incarnation_id, "rows": rows, "columns": columns,
        "cursor": { "row": rows - 1, "column": 0, "visible": true }, "title": live.runtime_id,
        "lines": lines, "next_sequence": state.store.index().map_err(ApiError::internal)?,
        "truncated": screen_line_count > TERMINAL_MAX_LINES
    });
    Ok((value, live))
}

pub(super) async fn terminal_screen(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "terminal.read")?;
    match terminal_screen_value(&state, &id, None) {
        Ok((screen, _)) => Ok(Json(screen)),
        Err(error) if error.code == "runtime-not-local" => {
            let subject = terminal_subject(&id);
            let status = state
                .store
                .status(Some(&subject))
                .map_err(ApiError::internal)?;
            let host = status
                .subjects
                .first()
                .and_then(|subject| subject.actual_origin.as_deref())
                .map(client_host_id)
                .ok_or_else(|| stale("the terminal owner is not known"))?;
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&host))?;
            if !session.authority_actor.starts_with("person/") {
                return Err(forbidden(
                    "remote terminal screen requires a concrete person",
                ));
            }
            let value = relay
                .read(
                    &host,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        request: crate::peer::ClientReadOperation::TerminalScreen {
                            terminal_id: client_detail_id("terminal", &id),
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&host, error))?;
            Ok(Json(value))
        }
        Err(error) => Err(error),
    }
}

#[derive(Default, Deserialize)]
pub(super) struct TerminalStreamQuery {
    after: Option<u64>,
    incarnation: Option<String>,
}

pub(super) async fn terminal_stream(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<TerminalStreamQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_scope(&session, "terminal.read")?;
    let subject = terminal_subject(&id);
    let protocols = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .collect::<Vec<_>>();
    let normative_count = protocols
        .iter()
        .filter(|protocol| **protocol == TERMINAL_SUBPROTOCOL)
        .count();
    let secondary = protocols
        .iter()
        .filter(|protocol| **protocol != TERMINAL_SUBPROTOCOL)
        .copied()
        .collect::<Vec<_>>();
    let stream_capability = secondary
        .first()
        .and_then(|protocol| protocol.strip_prefix(TERMINAL_CAPABILITY_PROTOCOL_PREFIX))
        .filter(|capability| !capability.is_empty());
    if normative_count != 1 || secondary.len() != 1 || stream_capability.is_none() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            code: "validation-failed".into(),
            message: format!(
                "the terminal WebSocket must request exactly `{TERMINAL_SUBPROTOCOL}` plus one `st3.cap.*` capability protocol"
            ),
            details: Box::default(),
        });
    }
    let live = match query.incarnation.as_deref() {
        Some(incarnation) => remote_terminal_live_session(&state, &subject, incarnation)?,
        None => terminal_live_session(&state, &subject, None)?,
    };
    if !live.terminal {
        return Err(validation(
            "the requested runtime does not expose a terminal",
        ));
    }
    if live.owner_host_id != client_host_id(&state.node)
        && !session.authority_actor.starts_with("person/")
    {
        return Err(forbidden(
            "remote terminal stream requires a concrete person",
        ));
    }
    let expected_incarnation = live.incarnation_id;
    consume_terminal_attachment(
        &state,
        &session,
        &client_detail_id("terminal", &id),
        &expected_incarnation,
        stream_capability,
    )?;
    if live.owner_host_id != client_host_id(&state.node) {
        let owner = live.owner_host_id;
        let authority_actor = session.authority_actor;
        return Ok(websocket
            .protocols([TERMINAL_SUBPROTOCOL])
            .on_upgrade(move |socket| {
                remote_terminal_stream_socket(
                    socket,
                    state,
                    id,
                    owner,
                    authority_actor,
                    expected_incarnation,
                    query.after,
                )
            }));
    }
    Ok(websocket
        .protocols([TERMINAL_SUBPROTOCOL])
        .on_upgrade(move |socket| {
            terminal_stream_socket(socket, state, id, expected_incarnation, query.after)
        }))
}

async fn remote_terminal_stream_socket(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    owner: String,
    authority_actor: String,
    incarnation: String,
    after: Option<u64>,
) {
    let Some(relay) = state.client_relay.as_ref() else {
        close_terminal_stream(&mut socket, 1012, "terminal owner unavailable").await;
        return;
    };
    let request = crate::peer::ClientReadRequest {
        authority_actor,
        request: crate::peer::ClientReadOperation::TerminalScreen {
            terminal_id: client_detail_id("terminal", &id),
        },
    };
    let screen = match relay.read(&owner, &request).await {
        Ok(screen) if screen["runtime_incarnation"].as_str() == Some(incarnation.as_str()) => {
            screen
        }
        _ => {
            close_terminal_stream(&mut socket, 1012, "terminal incarnation changed").await;
            return;
        }
    };
    if !send_terminal_stream_value(
        &mut socket,
        &terminal_stream_envelope(&state, screen.clone()),
    )
    .await
    {
        close_terminal_stream(
            &mut socket,
            1009,
            "terminal screen exceeds the client limit",
        )
        .await;
        return;
    }
    // Match the owner-local stream's second fence check after writing the first message.
    if relay.read(&owner, &request).await.map_or(true, |fresh| {
        fresh["runtime_incarnation"].as_str() != Some(incarnation.as_str())
    }) {
        close_terminal_stream(&mut socket, 1012, "terminal incarnation replaced").await;
        return;
    }
    let sequence = screen["next_sequence"].as_u64().unwrap_or_default();
    let frames = if after == Some(sequence) {
        Vec::new()
    } else {
        vec![json!({
            "id": format!("terminal-frame/{}/{}", id.trim_start_matches("terminal/"), sequence),
            "terminal_id": client_detail_id("terminal", &id),
            "runtime_incarnation": incarnation,
            "sequence": sequence,
            "type": "resync",
            "timestamp": client_timestamp(client_now_ms()),
            "body": { "screen": screen }
        })]
    };
    let page = terminal_stream_envelope(
        &state,
        json!({
            "kind": "terminal-frame-page",
            "terminal_id": client_detail_id("terminal", &id),
            "runtime_incarnation": incarnation,
            "frames": frames,
            "resume_sequence": sequence.saturating_add(1),
        }),
    );
    if !send_terminal_stream_value(&mut socket, &page).await {
        close_terminal_stream(
            &mut socket,
            1009,
            "terminal frame page exceeds the client limit",
        )
        .await;
        return;
    }
    close_terminal_stream(&mut socket, 1000, "terminal snapshot complete").await;
}

fn terminal_attachment_subject(id: &str) -> Result<String, ApiError> {
    id.strip_prefix("terminal-attachment/")
        .filter(|suffix| !suffix.is_empty() && !suffix.contains('/'))
        .map(|suffix| format!("custom/client/terminal-attachment-{suffix}"))
        .ok_or_else(|| validation("the terminal attachment ID is invalid"))
}

fn terminal_attachment_id(session: &ClientSession, request: &ActionRequest) -> String {
    let stable = hex::encode(Sha256::digest(
        format!("{}:{}", session.actor, request.idempotency_key).as_bytes(),
    ));
    format!("terminal-attachment/{}", &stable[..24])
}

fn existing_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
    request_digest: &str,
) -> Result<Option<Value>, ApiError> {
    let attachment_id = terminal_attachment_id(session, request);
    let subject = terminal_attachment_subject(&attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let Some(attached) = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
    else {
        return Ok(None);
    };
    if attached
        .body
        .pointer("/fields/request_digest")
        .and_then(Value::as_str)
        != Some(request_digest)
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "idempotency-conflict".into(),
            message: "the idempotency key was already used for a different action".into(),
            details: Box::default(),
        });
    }
    terminal_attachment_response(state, session, &attachment_id).map(Some)
}

fn terminal_capability_key(state: &AppState) -> Result<Vec<u8>, ApiError> {
    use std::io::{Read as _, Write as _};

    let path = state.state_dir.join("client-terminal.key");
    fn read_valid(path: &Path) -> Result<Vec<u8>, ApiError> {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(ApiError::internal)?;
        let metadata = file.metadata().map_err(ApiError::internal)?;
        if !metadata.file_type().is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(ApiError::internal(
                "the terminal capability key must be a daemon-owned 0600 regular file",
            ));
        }
        let mut key = Vec::with_capacity(32);
        file.read_to_end(&mut key).map_err(ApiError::internal)?;
        (key.len() == 32)
            .then_some(key)
            .ok_or_else(|| ApiError::internal("the terminal capability key is invalid"))
    }

    if path.exists() {
        return read_valid(&path);
    }
    fs::create_dir_all(&state.state_dir).map_err(ApiError::internal)?;
    let mut key = vec![0_u8; 32];
    getrandom::fill(&mut key).map_err(ApiError::internal)?;
    let mut staged = tempfile::Builder::new()
        .prefix(".client-terminal.key.")
        .tempfile_in(&state.state_dir)
        .map_err(ApiError::internal)?;
    staged
        .as_file_mut()
        .write_all(&key)
        .map_err(ApiError::internal)?;
    staged
        .as_file_mut()
        .sync_all()
        .map_err(ApiError::internal)?;
    match staged.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(&state.state_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(ApiError::internal)?;
            Ok(key)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => read_valid(&path),
        Err(error) => Err(ApiError::internal(error.error)),
    }
}

fn derive_terminal_capability(
    state: &AppState,
    session_actor: &str,
    attachment_id: &str,
    owner_host_id: &str,
) -> Result<String, ApiError> {
    let key = terminal_capability_key(state)?;
    // HMAC-SHA256 (RFC 2104) over the session-bound deterministic attachment identity.
    let mut block = [0_u8; 64];
    block[..key.len()].copy_from_slice(&key);
    let mut inner_key = [0x36_u8; 64];
    let mut outer_key = [0x5c_u8; 64];
    for index in 0..64 {
        inner_key[index] ^= block[index];
        outer_key[index] ^= block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(session_actor.as_bytes());
    inner.update([0]);
    inner.update(attachment_id.as_bytes());
    inner.update([0]);
    inner.update(owner_host_id.as_bytes());
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(outer.finalize()))
}

fn terminal_attachment_response(
    state: &AppState,
    session: &ClientSession,
    attachment_id: &str,
) -> Result<Value, ApiError> {
    let subject = terminal_attachment_subject(attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let attached = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
        .ok_or_else(|| ApiError::internal("the terminal attachment claim is missing"))?;
    if attached.origin != state.store.origin() {
        return Err(forbidden(
            "the terminal attachment belongs to another gateway",
        ));
    }
    let field = |name: &str| attached.body.pointer(&format!("/fields/{name}"));
    if field("session_actor").and_then(Value::as_str) != Some(session.actor.as_str()) {
        return Err(forbidden(
            "the terminal attachment belongs to another authenticated session",
        ));
    }
    let owner_host_id = field("owner_host_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no owner host"))?;
    if owner_host_id != client_host_id(&state.node)
        && state
            .client_relay
            .as_ref()
            .is_none_or(|relay| !relay.has_peer(owner_host_id))
    {
        return Err(remote_unavailable(owner_host_id));
    }
    let latest = claims
        .last()
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    let expires = field("expires_at_unix_ms")
        .and_then(Value::as_u64)
        .map(u128::from)
        .unwrap_or_default();
    let state_name = if latest.kind == "custom.client.terminal-detached" {
        "detached"
    } else if latest.kind == "custom.client.terminal-consumed" {
        "consumed"
    } else if expires <= client_now_ms() {
        "expired"
    } else {
        "available"
    };
    let capability = if state_name == "available" {
        Some(derive_terminal_capability(
            state,
            &session.actor,
            attachment_id,
            owner_host_id,
        )?)
    } else {
        None
    };
    let terminal_id = field("terminal_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no terminal ID"))?;
    let incarnation = field("runtime_incarnation")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no incarnation"))?;
    let routed = terminal_id.trim_start_matches("terminal/");
    Ok(json!({
        "attachment_id": attachment_id,
        "terminal_id": terminal_id,
        "runtime_incarnation": incarnation,
        "owner_host_id": owner_host_id,
        "stream_url": format!(
            "/v1/client/terminals/{}/stream?incarnation={}",
            urlencoding::encode(routed),
            urlencoding::encode(incarnation)
        ),
        "stream_capability": capability,
        "state": state_name,
        "expires_at": client_timestamp(expires),
    }))
}

fn create_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
    request_digest: &str,
) -> Result<Value, ApiError> {
    let target = parameter_string(&request.parameters, "target_id")?;
    let terminal_id = client_detail_id("terminal", &target);
    let incarnation = request
        .fence
        .runtime_incarnation
        .as_deref()
        .ok_or_else(|| validation("terminal attach requires a runtime incarnation fence"))?;
    let live = remote_terminal_live_session(state, &terminal_subject(&target), incarnation)?;
    if !live.terminal {
        return Err(validation("terminal attach requires a terminal runtime"));
    }
    // One idempotency key names exactly one attachment. The request digest is
    // persisted separately so a conflicting reuse cannot create an orphan on
    // another subject before the action receipt is published.
    let attachment_id = terminal_attachment_id(session, request);
    let subject = terminal_attachment_subject(&attachment_id)?;
    if let Some(existing) = existing_terminal_attachment(state, session, request, request_digest)? {
        return Ok(existing);
    }
    let capability =
        derive_terminal_capability(state, &session.actor, &attachment_id, &live.owner_host_id)?;
    let digest = credential_digest(&capability);
    let expires_at = client_now_ms() + 60_000;
    let appended = state.store.append_claim(&ClaimInput {
        subject,
        kind: "custom.client.terminal-attached".into(),
        actor: Some(session_claim_actor(session)),
        fields: BTreeMap::from([
            ("attachment_id".into(), Value::String(attachment_id.clone())),
            ("terminal_id".into(), Value::String(terminal_id.clone())),
            (
                "runtime_incarnation".into(),
                Value::String(incarnation.to_owned()),
            ),
            (
                "owner_host_id".into(),
                Value::String(live.owner_host_id.clone()),
            ),
            ("session_actor".into(), Value::String(session.actor.clone())),
            (
                "request_digest".into(),
                Value::String(request_digest.to_owned()),
            ),
            ("capability_hash".into(), Value::String(digest)),
            ("expires_at_unix_ms".into(), json!(expires_at)),
        ]),
        evidence: Vec::new(),
        expected_subject: Some(None),
        idempotency_key: None,
    });
    if let Err(error) = appended {
        if error.code != "stale-subject" {
            return Err(ApiError::bad(error));
        }
        let winner = state
            .store
            .claims_for(&terminal_attachment_subject(&attachment_id)?, None)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|claim| claim.kind == "custom.client.terminal-attached")
            .ok_or_else(|| ApiError::internal("attachment CAS lost without a winning claim"))?;
        if winner
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str)
            != Some(request_digest)
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
    }
    signal_changed(state);
    terminal_attachment_response(state, session, &attachment_id)
}

fn consume_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    terminal_id: &str,
    incarnation: &str,
    capability: Option<&str>,
) -> Result<(), ApiError> {
    let capability = capability
        .filter(|value| !value.is_empty())
        .ok_or_else(|| forbidden("a terminal stream capability is required"))?;
    let digest = credential_digest(capability);
    let claims = state
        .store
        .claims_page(None, None, 0, None, true, 100_000)
        .map_err(ApiError::internal)?;
    let attached = claims
        .claims
        .iter()
        .find(|claim| {
            claim.kind == "custom.client.terminal-attached"
                && claim
                    .body
                    .pointer("/fields/capability_hash")
                    .and_then(Value::as_str)
                    == Some(digest.as_str())
        })
        .ok_or_else(|| forbidden("the terminal stream capability is unknown"))?;
    let latest = claims
        .claims
        .iter()
        .filter(|claim| claim.subject == attached.subject)
        .max_by_key(|claim| claim.store_index)
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    let field = |name: &str| attached.body.pointer(&format!("/fields/{name}"));
    let valid = latest.id == attached.id
        && attached.origin == state.store.origin()
        && field("session_actor").and_then(Value::as_str) == Some(session.actor.as_str())
        && field("owner_host_id")
            .and_then(Value::as_str)
            .is_some_and(|owner| {
                owner == client_host_id(&state.node)
                    || state
                        .client_relay
                        .as_ref()
                        .is_some_and(|relay| relay.has_peer(owner))
            })
        && field("terminal_id").and_then(Value::as_str) == Some(terminal_id)
        && field("runtime_incarnation").and_then(Value::as_str) == Some(incarnation)
        && field("expires_at_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)
            .is_some_and(|expires| expires > client_now_ms());
    if !valid {
        return Err(forbidden(
            "the terminal stream capability is expired, consumed, detached, or belongs to another session",
        ));
    }
    state
        .store
        .append_claim(&ClaimInput {
            subject: attached.subject.clone(),
            kind: "custom.client.terminal-consumed".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([(
                "attachment_id".into(),
                field("attachment_id")
                    .cloned()
                    .ok_or_else(|| ApiError::internal("terminal attachment ID is missing"))?,
            )]),
            evidence: vec![attached.id.clone()],
            expected_subject: Some(Some(attached.id.clone())),
            idempotency_key: None,
        })
        .map_err(|_| forbidden("the terminal stream capability was already consumed"))?;
    signal_changed(state);
    Ok(())
}

fn detach_terminal_attachment(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<String, ApiError> {
    let attachment_id = parameter_string(&request.parameters, "target_id")?;
    let subject = terminal_attachment_subject(&attachment_id)?;
    let claims = state
        .store
        .claims_for(&subject, None)
        .map_err(ApiError::internal)?;
    let attached = claims
        .iter()
        .find(|claim| claim.kind == "custom.client.terminal-attached")
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "terminal attachment `{attachment_id}` does not exist"
            ))
        })?;
    if attached.origin != state.store.origin() {
        return Err(forbidden(
            "the terminal attachment belongs to another gateway",
        ));
    }
    let session_actor = attached
        .body
        .pointer("/fields/session_actor")
        .and_then(Value::as_str);
    let incarnation = attached
        .body
        .pointer("/fields/runtime_incarnation")
        .and_then(Value::as_str);
    if session_actor != Some(session.actor.as_str()) {
        return Err(forbidden(
            "the terminal attachment belongs to another authenticated session",
        ));
    }
    if incarnation != request.fence.runtime_incarnation.as_deref() {
        return Err(stale("the terminal attachment incarnation fence is stale"));
    }
    let latest = claims
        .last()
        .ok_or_else(|| ApiError::internal("the terminal attachment has no head"))?;
    if latest.kind == "custom.client.terminal-detached" {
        return Ok(attachment_id);
    }
    state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "custom.client.terminal-detached".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([(
                "attachment_id".into(),
                Value::String(attachment_id.clone()),
            )]),
            evidence: vec![latest.id.clone()],
            expected_subject: Some(Some(latest.id.clone())),
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(attachment_id)
}

fn terminal_stream_envelope(state: &AppState, value: Value) -> Value {
    json!({
        "api_version": CLIENT_API_VERSION,
        "request_id": format!("request/{}", new_request_id()),
        "snapshot": new_client_snapshot(state),
        "value": value,
    })
}

fn terminal_stream_error(error: &ApiError) -> Value {
    json!({
        "api_version": CLIENT_API_VERSION,
        "error_version": "st3.client.error.v0",
        "request_id": format!("request/{}", new_request_id()),
        "code": client_error_code(Some(&error.code)),
        "message": error.message,
        "retryable": false,
        "details": error.details,
    })
}

async fn send_terminal_stream_value(socket: &mut WebSocket, value: &Value) -> bool {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return false;
    };
    if bytes.len() > CLIENT_MAX_RESPONSE_BYTES {
        return false;
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return false;
    };
    socket.send(WsMessage::Text(text.into())).await.is_ok()
}

async fn close_terminal_stream(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(WsMessage::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

async fn terminal_stream_socket(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    expected_incarnation: String,
    after: Option<u64>,
) {
    let (screen, live) = match terminal_screen_value(&state, &id, Some(&expected_incarnation)) {
        Ok(value) => value,
        Err(error) => {
            let _ = send_terminal_stream_value(&mut socket, &terminal_stream_error(&error)).await;
            close_terminal_stream(&mut socket, 1008, "terminal incarnation changed").await;
            return;
        }
    };
    let screen_envelope = terminal_stream_envelope(&state, screen.clone());
    if !send_terminal_stream_value(&mut socket, &screen_envelope).await {
        close_terminal_stream(
            &mut socket,
            1009,
            "terminal screen exceeds the client limit",
        )
        .await;
        return;
    }

    // Revalidate after the screen write. A replacement racing the upgrade is never allowed to
    // append frames from a different incarnation to the atomic first message.
    if let Err(error) =
        terminal_live_session(&state, &terminal_subject(&id), Some(&expected_incarnation))
    {
        let _ = send_terminal_stream_value(&mut socket, &terminal_stream_error(&error)).await;
        close_terminal_stream(&mut socket, 1012, "terminal incarnation replaced").await;
        return;
    }

    let sequence = screen["next_sequence"].as_u64().unwrap_or_default();
    let frames = if after == Some(sequence) {
        Vec::new()
    } else {
        vec![json!({
            "id": format!("terminal-frame/{}/{}", id.trim_start_matches("terminal/"), sequence),
            "terminal_id": client_detail_id("terminal", &id),
            "runtime_incarnation": live.incarnation_id,
            "sequence": sequence,
            "type": "resync",
            "timestamp": client_timestamp(client_now_ms()),
            "body": { "screen": screen }
        })]
    };
    let page = terminal_stream_envelope(
        &state,
        json!({
            "kind": "terminal-frame-page",
            "terminal_id": client_detail_id("terminal", &id),
            "runtime_incarnation": live.incarnation_id,
            "frames": frames,
            "resume_sequence": sequence.saturating_add(1)
        }),
    );
    if !send_terminal_stream_value(&mut socket, &page).await {
        close_terminal_stream(
            &mut socket,
            1009,
            "terminal frame page exceeds the client limit",
        )
        .await;
        return;
    }
    close_terminal_stream(&mut socket, 1000, "terminal snapshot complete").await;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fence {
    snapshot_id: String,
    #[serde(default)]
    subject_revisions: BTreeMap<String, String>,
    mission_generation: Option<String>,
    step_definition: Option<String>,
    attempt: Option<u32>,
    readiness_epoch: Option<u64>,
    runtime_incarnation: Option<String>,
    terminal_sequence: Option<u64>,
    preview_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionRequest {
    api_version: String,
    id: String,
    #[serde(rename = "type")]
    action_type: String,
    idempotency_key: String,
    fence: Fence,
    parameters: Value,
}

fn action_scope(action: &str) -> Option<&'static str> {
    Some(match action.split_once('.')?.0 {
        "attention" => "control.attention",
        "review" => "control.attention",
        "message" => "control.messages",
        "launch" => "control.launches",
        "mission" => "control.missions",
        "session" => "control.missions",
        "work" => "control.work",
        "runtime" => "control.runtimes",
        "terminal" => {
            if matches!(action, "terminal.attach" | "terminal.detach") {
                "terminal.read"
            } else {
                "terminal.control"
            }
        }
        "pairing" => "control.pairing",
        _ => return None,
    })
}

fn parameter_string(parameters: &Value, key: &str) -> Result<String, ApiError> {
    parameters
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| validation(format!("action parameters require `{key}`")))
}

fn validate_message_session(
    state: &AppState,
    snapshot: &ClientSnapshot,
    recipient: &str,
    session_id: &str,
) -> Result<(), ApiError> {
    let recipient = normalize_message_party(recipient);
    let current_session = client_agent_resources(
        &state.store,
        false,
        &snapshot.created_at,
        snapshot.store_index,
    )
    .map_err(ApiError::internal)?
    .into_iter()
    .find(|agent| agent["id"] == recipient)
    .and_then(|agent| agent["current_session_id"].as_str().map(str::to_owned))
    .ok_or_else(|| {
        validation(format!(
            "message recipient `{recipient}` has no current normalized session"
        ))
    })?;
    if current_session != session_id {
        return Err(stale(format!(
            "session `{session_id}` is not the current session for `{recipient}`"
        )));
    }
    Ok(())
}

async fn import_external_session_action(
    state: &AppState,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    let target = parameter_string(&request.parameters, "target_id")?;
    let external =
        crate::external_sessions::find_fresh(state.native_session_home.as_deref(), &target)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::not_found(format!("external session `{target}` does not exist"))
            })?;
    if request.fence.subject_revisions.get(&target) != Some(&external.revision) {
        return Err(stale("the native session changed before import"));
    }
    if external.process.is_none() {
        let discovery =
            crate::external_sessions::discover_fresh(state.native_session_home.as_deref(), false)
                .map_err(ApiError::internal)?;
        if discovery.unresolved_processes.iter().any(|candidate| {
            candidate.driver == external.driver
                && candidate.process.cwd.as_ref() == external.cwd.as_ref()
        }) {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "ambiguous-running-session".into(),
                message: "a running harness in this workspace does not expose its exact native session ID; stop it before importing the saved session".into(),
                details: Box::new(serde_json::Map::from_iter([(
                    "target_id".into(),
                    Value::String(target),
                )])),
            });
        }
    }

    let import = crate::external_sessions::import_seat(&external).map_err(|error| {
        ApiError::bad(St3Error::new("invalid-import-session", error.to_string()))
    })?;
    let seat_intent = parse_intent(&import.kdl, &state.node).map_err(ApiError::bad)?;
    let seat_preview = state
        .store
        .mission(
            &seat_intent,
            IntentInput {
                kdl: import.kdl.clone(),
                source_name: Some(format!("session import seat {target}")),
            },
        )
        .map_err(ApiError::bad)?;
    if !seat_preview.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-import-seat",
            seat_preview.blockers.join("; "),
        )));
    }

    // Fenced takeover is deliberately stop -> declare -> start. The graph intent is fully parsed
    // and authorized before the predecessor is touched, but the durable seat is not made desired
    // until the exact native process has stopped, so two harnesses never own one native session.
    if let Some(process) = external.process.clone() {
        let driver = external.driver;
        tokio::task::spawn_blocking(move || {
            crate::external_sessions::terminate_exact_process(driver, &process)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    }
    state
        .store
        .apply_as(
            &seat_intent,
            &seat_preview.subject_tokens,
            &format!("{}:seat", request.idempotency_key),
            Some(&session.authority_actor),
        )
        .map_err(ApiError::bad)?;
    state
        .store
        .append_claim(&ClaimInput {
            subject: import.subject.clone(),
            kind: "harness.session-file".into(),
            actor: Some(session_claim_actor(session)),
            fields: BTreeMap::from([
                (
                    "harness".into(),
                    Value::String(external.driver.as_str().into()),
                ),
                (
                    "path".into(),
                    Value::String(external.transcript.to_string_lossy().into_owned()),
                ),
                (
                    "session_id".into(),
                    Value::String(external.native_id.clone()),
                ),
                ("source_session".into(), Value::String(external.id.clone())),
                (
                    "discovery_revision".into(),
                    Value::String(external.revision.clone()),
                ),
                ("agent".into(), Value::String(import.subject.clone())),
                ("status".into(), Value::String("unknown".into())),
                (
                    "modified_at".into(),
                    Value::String(crate::external_sessions::timestamp(
                        external.updated_at_unix_ms,
                    )),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("{}:native-session", request.idempotency_key)),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(vec![external.id, import.subject])
}

fn contains_identity_selector(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "actor" | "credential" | "fleet_secret" | "shared_secret"
            ) || contains_identity_selector(value)
        }),
        Value::Array(values) => values.iter().any(contains_identity_selector),
        _ => false,
    }
}

fn validate_work_fence(state: &AppState, target: &str, fence: &Fence) -> Result<(), ApiError> {
    let work = state
        .store
        .work(None, true)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|work| work.subject == target || work.subject == client_detail_id("step-run", target))
        .ok_or_else(|| ApiError::not_found(format!("work `{target}` does not exist")))?;
    if fence.mission_generation.as_deref() != Some(work.generation.as_str())
        || fence.step_definition.as_deref() != Some(work.definition_hash.as_str())
        || fence.attempt != Some(work.attempt)
        || fence.readiness_epoch != Some(u64::from(work.readiness_epoch))
        || fence
            .runtime_incarnation
            .as_deref()
            .is_some_and(|incarnation| work.claim_incarnation.as_deref() != Some(incarnation))
    {
        return Err(stale(format!(
            "the execution fence for `{}` is stale",
            work.subject
        )));
    }
    Ok(())
}

fn validate_launch_fence(state: &AppState, target: &str, fence: &Fence) -> Result<(), ApiError> {
    let launch_id = launch_session_id(target);
    let launch = state
        .store
        .planning_session(launch_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("launch `{target}` does not exist")))?;
    let resource_id = format!("launch/{}", launch.id);
    let expected = format!("launch/{}", launch.updated_at_unix_ms);
    if fence.subject_revisions.get(&resource_id) != Some(&expected) {
        return Err(stale(format!(
            "the launch revision fence for `{resource_id}` is stale"
        )));
    }
    Ok(())
}

fn validate_fence(
    state: &AppState,
    _snapshot: &ClientSnapshot,
    fence: &Fence,
) -> Result<(), ApiError> {
    let parsed = fence
        .snapshot_id
        .strip_prefix("snapshot/")
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(host_and_index, _fingerprint)| host_and_index.rsplit_once('/'));
    let current_index = state.store.index().map_err(ApiError::internal)?;
    let expected_host = state.node.replace(char::is_whitespace, "-");
    if !parsed.is_some_and(|(host, index)| {
        host == expected_host && index.parse::<u64>().ok() == Some(current_index)
    }) {
        return Err(stale(
            "the client snapshot changed before the action was submitted",
        ));
    }
    for (subject, revision) in &fence.subject_revisions {
        let current = if let Some(id) = subject.strip_prefix("launch/") {
            state
                .store
                .planning_session(id)
                .map_err(ApiError::internal)?
                .map(|launch| format!("launch/{}", launch.updated_at_unix_ms))
        } else {
            state
                .store
                .claims_for(subject, None)
                .map_err(ApiError::internal)?
                .last()
                .map(|claim| claim.id.clone())
        };
        if current.as_deref() != Some(revision) {
            return Err(stale(format!(
                "the revision fence for `{subject}` is stale"
            )));
        }
    }
    Ok(())
}

async fn dispatch_action(
    state: &AppState,
    snapshot: &ClientSnapshot,
    session: &ClientSession,
    request: &ActionRequest,
) -> Result<Vec<String>, ApiError> {
    let p = &request.parameters;
    let authority_actor = &session.authority_actor;
    match request.action_type.as_str() {
        decision @ ("review.approve" | "review.reject") => {
            let target = parameter_string(p, "target_id")?;
            let result = post_review(
                State(state.clone()),
                AxumPath(target),
                Json(ReviewRequest {
                    decision: if decision == "review.approve" {
                        "approved".into()
                    } else {
                        "rejected".into()
                    },
                    reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                    actor: Some(authority_actor.clone()),
                    expected_subject: None,
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "attention.resolve" => {
            let target = parameter_string(p, "attention_id")?;
            let attention = state
                .store
                .attention_request(&target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("attention `{target}` does not exist"))
                })?;
            if attention.reviewer != *authority_actor {
                return Err(forbidden(format!(
                    "attention `{target}` belongs to another person"
                )));
            }
            let result = resolve_attention(
                State(state.clone()),
                AxumPath(target),
                Json(AttentionResolveRequest {
                    outcome: parameter_string(p, "outcome")?,
                    reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                    actor: authority_actor.clone(),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "message.send" => {
            let to = parameter_string(p, "to")?;
            let session_id = p
                .get("session_id")
                .map(|_| parameter_string(p, "session_id"))
                .transpose()?;
            if let Some(session_id) = session_id.as_deref() {
                validate_message_session(state, snapshot, &to, session_id)?;
            }
            let result = accept_message(
                state,
                MessageSendRequest {
                    idempotency_key: request.idempotency_key.clone(),
                    from: authority_actor.clone(),
                    to,
                    content: parameter_string(p, "content")?,
                    title: p.get("title").and_then(Value::as_str).map(str::to_owned),
                    in_reply_to: p
                        .get("in_reply_to")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    tags: p
                        .get("tags")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                },
                session_id,
            )?
            .0;
            Ok(vec![result.subject])
        }
        "message.read" | "message.close" => {
            let target = parameter_string(p, "target_id")?;
            let lifecycle = if request.action_type == "message.read" {
                "read"
            } else {
                "closed"
            };
            let result = post_message_claim(
                State(state.clone()),
                AxumPath(target),
                Json(MessageLifecycleRequest {
                    lifecycle: lifecycle.into(),
                    actor: Some(authority_actor.clone()),
                    transport: None,
                    runtime_id: None,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "launch.create" => {
            let target = p
                .get("target")
                .and_then(Value::as_object)
                .ok_or_else(|| validation("launch creation requires a typed target"))?;
            let target_type = target
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| validation("launch target requires `type`"))?;
            let (mission, run, workspace) = match target_type {
                "new-mission" => (
                    target
                        .get("mission_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a new-mission launch target requires `mission_id`")
                        })?
                        .trim_start_matches("mission/")
                        .to_owned(),
                    None,
                    target
                        .get("workspace")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a new-mission launch target requires `workspace`")
                        })?
                        .to_owned(),
                ),
                "mission-run" => {
                    let run_id = target
                        .get("mission_run_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a mission-run launch target requires `mission_run_id`")
                        })?;
                    let current = state
                        .store
                        .mission_run(run_id)
                        .map_err(ApiError::internal)?
                        .ok_or_else(|| {
                            ApiError::not_found(format!("mission run `{run_id}` does not exist"))
                        })?;
                    let generation = target
                        .get("generation_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            validation("a mission-run launch target requires `generation_id`")
                        })?;
                    if generation != current.generation
                        || request.fence.mission_generation.as_deref()
                            != Some(current.generation.as_str())
                    {
                        return Err(stale("the launch target generation is stale"));
                    }
                    (
                        current.mission.trim_start_matches("mission/").to_owned(),
                        Some(current.subject),
                        current.workspace,
                    )
                }
                _ => return Err(validation("launch target type is invalid")),
            };
            let launch = start_planning_session(
                State(state.clone()),
                Json(PlanningSessionStartRequest {
                    mission,
                    run,
                    request: parameter_string(p, "request")?.into_bytes(),
                    workspace,
                    requester: Some(authority_actor.clone()),
                    provider: p.get("provider").and_then(Value::as_str).map(str::to_owned),
                    model: p.get("model").and_then(Value::as_str).map(str::to_owned),
                    effort: p.get("effort").and_then(Value::as_str).map(str::to_owned),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.revise" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let launch = revise_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningRevisionRequest {
                    actor: authority_actor.clone(),
                    feedback: parameter_string(p, "feedback")?.into_bytes(),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.preview" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let variant_id = parameter_string(p, "variant_id")?;
            let variant = variant_id.rsplit('/').next().unwrap_or(&variant_id);
            let launch = preview_named_planning_candidate(
                State(state.clone()),
                AxumPath((launch_session_id(&launch_id).to_owned(), variant.to_owned())),
            )
            .await?
            .0;
            Ok(vec![
                format!("launch/{}", launch.id),
                format!("launch-variant/{}/{}", launch.id, variant),
            ])
        }
        "launch.approve" => {
            let launch_id = parameter_string(p, "launch_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let requested_variant = parameter_string(p, "variant_id")?;
            let requested_variant = requested_variant
                .rsplit('/')
                .next()
                .unwrap_or(&requested_variant);
            let launch = state
                .store
                .planning_session(launch_session_id(&launch_id))
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("launch `{launch_id}` does not exist"))
                })?;
            if launch
                .candidate
                .as_ref()
                .map(|candidate| candidate.variant.as_str())
                != Some(requested_variant)
            {
                return Err(stale("the selected launch variant is stale"));
            }
            let launch = approve_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningApprovalRequest {
                    actor: authority_actor.clone(),
                    preview_hash: request.fence.preview_token.clone().ok_or_else(|| {
                        validation("launch approval requires a preview token fence")
                    })?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        "launch.cancel" => {
            let launch_id = parameter_string(p, "target_id")?;
            validate_launch_fence(state, &launch_id, &request.fence)?;
            let launch = cancel_planning_session(
                State(state.clone()),
                AxumPath(launch_session_id(&launch_id).to_owned()),
                Json(PlanningCancelRequest {
                    actor: authority_actor.clone(),
                    reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![format!("launch/{}", launch.id)])
        }
        action @ ("work.claim" | "work.renew" | "work.progress" | "work.complete" | "work.fail"
        | "work.release") => {
            let target = parameter_string(p, "target_id")?;
            validate_work_fence(state, &target, &request.fence)?;
            let result = state
                .store
                .work_action(
                    &target,
                    action.trim_start_matches("work."),
                    &WorkRequest {
                        actor: Some(authority_actor.clone()),
                        incarnation: request.fence.runtime_incarnation.clone(),
                        summary: p.get("summary").and_then(Value::as_str).map(str::to_owned),
                        reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                        evidence: p
                            .get("evidence")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        idempotency_key: request.idempotency_key.clone(),
                    },
                )
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![result.subject])
        }
        "mission.start" => {
            let inputs = p
                .get("inputs")
                .and_then(Value::as_object)
                .ok_or_else(|| validation("mission start requires an `inputs` object"))?
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_owned()))
                        .ok_or_else(|| validation("mission input values must be strings"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let result = start_mission_run_action(
                State(state.clone()),
                Json(MissionRunRequest {
                    mission: parameter_string(p, "mission_id")?,
                    revision: None,
                    workspace: parameter_string(p, "workspace")?,
                    requester: Some(authority_actor.clone()),
                    mode: Some("run".into()),
                    inputs,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "mission.cancel" => {
            let target = parameter_string(p, "target_id")?;
            let current = state
                .store
                .mission_run(&target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("mission run `{target}` does not exist"))
                })?;
            if request.fence.mission_generation.as_deref() != Some(current.generation.as_str()) {
                return Err(stale("the mission generation fence is stale"));
            }
            let reason = p
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("the mission was cancelled by its requester");
            state
                .store
                .request_mission_run_cancellation(&current.subject, reason)
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![current.subject])
        }
        decision @ ("mission.approve-revision" | "mission.cancel-revision") => {
            let target = parameter_string(p, "target_id")?;
            let proposal = state
                .store
                .revision_proposal(&target)
                .map_err(ApiError::internal)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("revision proposal `{target}` does not exist"))
                })?;
            if request.fence.mission_generation.as_deref()
                != Some(proposal.source_generation.as_str())
            {
                return Err(stale("the revision proposal generation fence is stale"));
            }
            if decision == "mission.approve-revision" {
                let result = approve_revision_proposal(
                    State(state.clone()),
                    AxumPath(target),
                    Json(RevisionApprovalRequest {
                        actor: authority_actor.clone(),
                        preview_hash: request.fence.preview_token.clone().ok_or_else(|| {
                            validation("revision approval requires a preview token fence")
                        })?,
                        idempotency_key: request.idempotency_key.clone(),
                    }),
                )
                .await?
                .0;
                Ok(vec![result.mission_run.subject])
            } else {
                let result = cancel_revision_proposal(
                    State(state.clone()),
                    AxumPath(target),
                    Json(RevisionCancelRequest {
                        actor: authority_actor.clone(),
                        reason: p.get("reason").and_then(Value::as_str).map(str::to_owned),
                        idempotency_key: request.idempotency_key.clone(),
                    }),
                )
                .await?
                .0;
                Ok(vec![result.subject])
            }
        }
        "session.import" => import_external_session_action(state, session, request).await,
        "terminal.input" => {
            let mode = match parameter_string(p, "mode")?.as_str() {
                "line" => SessionInputMode::Line,
                "raw" => SessionInputMode::Raw,
                "key" => SessionInputMode::Key,
                _ => return Err(validation("terminal input mode is invalid")),
            };
            let target = terminal_subject(&parameter_string(p, "terminal_id")?);
            let result = input_session_as(
                state,
                target,
                SessionInputRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("terminal input requires a runtime incarnation fence"),
                    )?,
                    mode,
                    value: parameter_string(p, "value")?,
                    idempotency_key: request.idempotency_key.clone(),
                },
                authority_actor,
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "terminal.resize" => {
            let target = terminal_subject(&parameter_string(p, "terminal_id")?);
            let rows = p
                .get("rows")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0)
                .ok_or_else(|| validation("terminal resize rows must fit a positive u16"))?;
            let columns = p
                .get("columns")
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0)
                .ok_or_else(|| validation("terminal resize columns must fit a positive u16"))?;
            let live = live_session(state, &target, request.fence.runtime_incarnation.as_deref())?;
            if !live.terminal {
                return Err(validation("terminal resize requires a terminal runtime"));
            }
            let socket = state.pty_root.join(format!("{}.sock", live.runtime_id));
            let runtime_id = live.runtime_id.clone();
            tokio::task::spawn_blocking(move || {
                let stream = std::os::unix::net::UnixStream::connect(&socket)?;
                let mut connection = pty_core::client::SessionConnection::attach_over(
                    stream,
                    &runtime_id,
                    rows,
                    columns,
                    Some(Duration::from_secs(2)),
                )?;
                connection.resize(rows, columns);
                connection.disconnect();
                anyhow::Ok(())
            })
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
            Ok(vec![client_detail_id(
                "terminal",
                &parameter_string(p, "terminal_id")?,
            )])
        }
        "terminal.detach" => Ok(vec![detach_terminal_attachment(state, session, request)?]),
        "runtime.context-clear" => {
            let target = terminal_subject(&parameter_string(p, "target_id")?);
            let result = clear_context(
                State(state.clone()),
                AxumPath(target),
                Json(ContextClearRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("runtime control requires an incarnation fence"),
                    )?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "runtime.signal" => {
            let target = terminal_subject(&parameter_string(p, "target_id")?);
            let result = signal_session(
                State(state.clone()),
                AxumPath(target),
                Json(SessionSignalRequest {
                    expected_incarnation: request.fence.runtime_incarnation.clone().ok_or_else(
                        || validation("runtime control requires an incarnation fence"),
                    )?,
                    signal: parameter_string(p, "signal")?,
                    idempotency_key: request.idempotency_key.clone(),
                }),
            )
            .await?
            .0;
            Ok(vec![result.subject])
        }
        "pairing.revoke" => {
            let device = parameter_string(p, "target_id")?;
            let claims = state
                .store
                .claims_for_kind_at("custom.client.pairing-completed", None, true, 10_000)
                .map_err(ApiError::internal)?;
            let paired = claims
                .claims
                .iter()
                .find(|claim| {
                    claim
                        .body
                        .pointer("/fields/device_id")
                        .and_then(Value::as_str)
                        == Some(device.as_str())
                })
                .ok_or_else(|| {
                    ApiError::not_found(format!("paired device `{device}` does not exist"))
                })?;
            state
                .store
                .append_claim(&ClaimInput {
                    subject: paired.subject.clone(),
                    kind: "custom.client.pairing-revoked".into(),
                    actor: Some(authority_actor.clone()),
                    fields: BTreeMap::from([("device_id".into(), Value::String(device.clone()))]),
                    evidence: vec![paired.id.clone()],
                    expected_subject: None,
                    idempotency_key: Some(request.idempotency_key.clone()),
                })
                .map_err(ApiError::bad)?;
            signal_changed(state);
            Ok(vec![device])
        }
        _ => Err(ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            code: "unsupported-capability".into(),
            message: format!(
                "action `{}` is declared but is not available on this daemon",
                request.action_type
            ),
            details: Box::default(),
        }),
    }
}

pub(super) async fn action(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>,
    Json(request): Json<ActionRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.api_version != CLIENT_API_VERSION
        || !request.id.starts_with("action/")
        || !(16..=256).contains(&request.idempotency_key.len())
    {
        return Err(validation(
            "the action version, ID, or idempotency key is invalid",
        ));
    }
    if contains_identity_selector(&request.parameters) {
        return Err(validation(
            "client actions cannot select an actor, credential, or fleet secret",
        ));
    }
    let scope = action_scope(&request.action_type)
        .ok_or_else(|| validation("the action type is unknown"))?;
    require_scope(&session, scope)?;
    let read_only_terminal_lifecycle = matches!(
        request.action_type.as_str(),
        "terminal.attach" | "terminal.detach"
    );
    if !read_only_terminal_lifecycle && !session.authority_actor.starts_with("person/") {
        return Err(forbidden(
            "client mutations require explicit concrete person authority",
        ));
    }
    let encoded = serde_json::to_vec(&request).map_err(ApiError::internal)?;
    let request_digest = hex::encode(Sha256::digest(&encoded));
    let receipt_digest = hex::encode(Sha256::digest(
        format!("{}:{}", session.actor, request.idempotency_key).as_bytes(),
    ));
    let receipt_subject = format!("custom/client/action-{}", &receipt_digest[..32]);
    if let Some(receipt) = state
        .store
        .claims_for(&receipt_subject, Some("custom.client.action-result"))
        .map_err(ApiError::internal)?
        .last()
    {
        let old_digest = receipt
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str);
        if old_digest != Some(request_digest.as_str()) {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
        let mut result = receipt
            .body
            .pointer("/fields/result")
            .cloned()
            .ok_or_else(|| ApiError::internal("the client action receipt has no result"))?;
        if request.action_type == "terminal.attach" {
            let attachment_id = result["affected_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("the terminal attach receipt has no ID"))?;
            result["terminal_attachment"] =
                terminal_attachment_response(&state, &session, attachment_id)?;
        }
        result["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
        return Ok(Json(result));
    }
    if matches!(
        request.action_type.as_str(),
        "terminal.input" | "terminal.resize"
    ) {
        let terminal_id = parameter_string(&request.parameters, "terminal_id")?;
        let incarnation = request
            .fence
            .runtime_incarnation
            .as_deref()
            .ok_or_else(|| validation("terminal control requires an incarnation fence"))?;
        let live =
            remote_terminal_live_session(&state, &terminal_subject(&terminal_id), incarnation)?;
        if live.owner_host_id != client_host_id(&state.node) {
            validate_fence(&state, &snapshot, &request.fence)?;
            let expected_sequence = request
                .fence
                .terminal_sequence
                .ok_or_else(|| validation("terminal control requires a sequence fence"))?;
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&live.owner_host_id))?;
            let mut value = relay
                .read(
                    &live.owner_host_id,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        request: crate::peer::ClientReadOperation::TerminalControl {
                            action_id: request.id.clone(),
                            idempotency_key: request.idempotency_key.clone(),
                            action_type: request.action_type.clone(),
                            terminal_id: client_detail_id("terminal", &terminal_id),
                            runtime_incarnation: incarnation.into(),
                            expected_sequence,
                            parameters: request.parameters.clone(),
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&live.owner_host_id, error))?;
            value["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
            return Ok(Json(value));
        }
    }
    let mut reconciled_attachment = None;
    let fence_result = (|| {
        validate_fence(&state, &snapshot, &request.fence)?;
        if request.action_type.starts_with("terminal.")
            && request.action_type != "terminal.detach"
            && request.fence.terminal_sequence
                != Some(state.store.index().map_err(ApiError::internal)?)
        {
            return Err(stale("the terminal sequence fence is stale"));
        }
        Ok(())
    })();
    if let Err(error) = fence_result {
        if request.action_type == "terminal.attach" {
            reconciled_attachment =
                existing_terminal_attachment(&state, &session, &request, &request_digest)?;
        }
        if reconciled_attachment.is_none() {
            return Err(error);
        }
    }
    let terminal_attachment = if request.action_type == "terminal.attach" {
        let target = parameter_string(&request.parameters, "target_id")?;
        let incarnation = request
            .fence
            .runtime_incarnation
            .as_deref()
            .ok_or_else(|| validation("terminal attach requires an incarnation fence"))?;
        let live = remote_terminal_live_session(&state, &terminal_subject(&target), incarnation)?;
        if live.owner_host_id != client_host_id(&state.node) && reconciled_attachment.is_none() {
            if !session.authority_actor.starts_with("person/") {
                return Err(forbidden(
                    "remote terminal attach requires a concrete person",
                ));
            }
            let relay = state
                .client_relay
                .as_ref()
                .ok_or_else(|| remote_unavailable(&live.owner_host_id))?;
            let screen = relay
                .read(
                    &live.owner_host_id,
                    &crate::peer::ClientReadRequest {
                        authority_actor: session.authority_actor.clone(),
                        request: crate::peer::ClientReadOperation::TerminalScreen {
                            terminal_id: client_detail_id("terminal", &target),
                        },
                    },
                )
                .await
                .map_err(|error| remote_read_error(&live.owner_host_id, error))?;
            if screen["runtime_incarnation"].as_str() != Some(incarnation) {
                return Err(stale("the terminal incarnation fence is stale"));
            }
        }
        Some(if let Some(existing) = reconciled_attachment {
            existing
        } else {
            create_terminal_attachment(&state, &session, &request, &request_digest)?
        })
    } else {
        None
    };
    let affected = if let Some(attachment) = &terminal_attachment {
        vec![
            attachment["attachment_id"]
                .as_str()
                .ok_or_else(|| ApiError::internal("terminal attachment has no ID"))?
                .to_owned(),
        ]
    } else {
        dispatch_action(&state, &snapshot, &session, &request).await?
    };
    let operation_id = format!("operation/client-{}", &request_digest[..24]);
    let mut result = json!({ "kind": "action-result", "action_id": request.id, "operation_id": operation_id, "status": "completed", "affected_ids": affected });
    if let Some(attachment) = terminal_attachment {
        result["terminal_attachment"] = attachment;
    }
    let mut persisted_result = result.clone();
    persisted_result
        .as_object_mut()
        .expect("action result is an object")
        .remove("terminal_attachment");
    let receipt_write = state.store.append_claim(&ClaimInput {
        subject: receipt_subject.clone(),
        kind: "custom.client.action-result".into(),
        actor: Some(session_claim_actor(&session)),
        fields: BTreeMap::from([
            (
                "authority_actor".into(),
                Value::String(session.authority_actor.clone()),
            ),
            (
                "request_digest".into(),
                Value::String(request_digest.clone()),
            ),
            ("result".into(), persisted_result.clone()),
        ]),
        evidence: Vec::new(),
        expected_subject: Some(None),
        idempotency_key: None,
    });
    if let Err(error) = receipt_write {
        if error.code != "stale-subject" {
            return Err(ApiError::bad(error));
        }
        let receipt = state
            .store
            .claims_for(&receipt_subject, Some("custom.client.action-result"))
            .map_err(ApiError::internal)?
            .into_iter()
            .last()
            .ok_or_else(|| ApiError::internal("action receipt CAS lost without a winner"))?;
        if receipt
            .body
            .pointer("/fields/request_digest")
            .and_then(Value::as_str)
            != Some(request_digest.as_str())
        {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                code: "idempotency-conflict".into(),
                message: "the idempotency key was already used for a different action".into(),
                details: Box::default(),
            });
        }
        let mut replay = receipt
            .body
            .pointer("/fields/result")
            .cloned()
            .ok_or_else(|| ApiError::internal("the client action receipt has no result"))?;
        if request.action_type == "terminal.attach" {
            let attachment_id = replay["affected_ids"]
                .as_array()
                .and_then(|ids| ids.first())
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("the terminal attach receipt has no ID"))?;
            replay["terminal_attachment"] =
                terminal_attachment_response(&state, &session, attachment_id)?;
        }
        replay["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
        return Ok(Json(replay));
    }
    signal_changed(&state);
    result["snapshot_id"] = Value::String(new_client_snapshot(&state).id);
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;
    use std::sync::Barrier;

    fn test_state(root: &Path) -> AppState {
        test_state_named(root, "terminal-test")
    }

    fn test_state_named(root: &Path, node: &str) -> AppState {
        AppState {
            store: Arc::new(Store::open(&root.join("graph.db"), node).unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: crate::model::PlannerSpec::default(),
        }
    }

    #[test]
    fn device_projection_uses_paired_device_name() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "custom/client/pairing-named";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-begun".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("pairing_id".into(), Value::String("pairing/named".into())),
                    (
                        "device_name".into(),
                        Value::String("Nathan's iPhone".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("device_id".into(), Value::String("device/named".into())),
                    ("person_id".into(), Value::String("person/nathan".into())),
                    (
                        "session_actor".into(),
                        Value::String("person/nathan/session/named".into()),
                    ),
                    (
                        "expires_at_unix_ms".into(),
                        Value::from(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let resources =
            device_resources(&state, &new_client_snapshot(&state), "person/nathan").unwrap();
        assert_eq!(resources[0]["name"], "Nathan's iPhone");
    }

    #[test]
    fn paired_authentication_uses_pairing_claims_and_honors_revocation() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        let subject = "custom/client/pairing-auth-test";
        let credential = "pairing-auth-test-secret";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-completed".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    (
                        "credential_hash".into(),
                        Value::String(credential_digest(credential)),
                    ),
                    (
                        "session_actor".into(),
                        Value::String("client/test-session".into()),
                    ),
                    ("person_id".into(), Value::String("person/nathan".into())),
                    ("scopes".into(), json!(["read.projections"])),
                    (
                        "expires_at_unix_ms".into(),
                        json!(client_now_ms() as u64 + 60_000),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let request = Request::builder()
            .uri("/v1/client/agents")
            .header(AUTHORIZATION, format!("Bearer {credential}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            authenticate(&state, &request, "fabric-loopback")
                .unwrap()
                .actor,
            "client/test-session"
        );
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(authenticate(&state, &request, "fabric-loopback").is_err());
    }

    #[test]
    fn mission_resources_include_published_definitions_without_runs() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "zero-run-node");
        let source = r#"version 2
mission "example/zero-run" state="ready" {
  goal "Remain visible before the first run starts."
}
"#;
        let intent = crate::graph::parse_intent(source, "zero-run-node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: Some("zero-run.kdl".into()),
                },
            )
            .unwrap();
        state
            .store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "publish-zero-run",
                Some("person/operator"),
            )
            .unwrap();

        let resources =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        let mission = resources
            .iter()
            .find(|value| value["id"] == "mission/example/zero-run")
            .expect("the zero-run definition is listed");
        assert_eq!(mission["state"], "ready");
        assert_eq!(mission["runs"], json!([]));
        assert_eq!(mission["operational"]["actionable"], true);
        assert!(mission["visualization"].is_null());
        assert_eq!(
            mission["mission_revision"],
            intent.missions["example/zero-run"].revision
        );
        let details = mission_resources(
            &state.store,
            state.store.index().unwrap(),
            true,
            Some("mission/example/zero-run"),
        )
        .unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(
            details[0]["visualization"]["mission"],
            "mission/example/zero-run"
        );

        let retired_source = source.replace("state=\"ready\"", "state=\"retired\"");
        let retired = crate::graph::parse_intent(&retired_source, "zero-run-node").unwrap();
        let retired_preview = state
            .store
            .mission(
                &retired,
                crate::model::IntentInput {
                    kdl: retired_source,
                    source_name: Some("retired-zero-run.kdl".into()),
                },
            )
            .unwrap();
        state
            .store
            .apply_as(
                &retired,
                &retired_preview.subject_tokens,
                "retire-zero-run",
                Some("person/operator"),
            )
            .unwrap();
        let current =
            mission_resources(&state.store, state.store.index().unwrap(), false, None).unwrap();
        assert!(
            current
                .iter()
                .all(|value| value["id"] != "mission/example/zero-run")
        );
        let retired_resources =
            mission_resources(&state.store, state.store.index().unwrap(), true, None).unwrap();
        let retired_mission = retired_resources
            .iter()
            .find(|value| value["id"] == "mission/example/zero-run")
            .unwrap();
        assert_eq!(retired_mission["state"], "retired");
        assert_eq!(retired_mission["operational"]["actionable"], false);
    }

    #[tokio::test]
    async fn a_saved_native_session_import_declares_one_durable_resuming_seat() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        let transcript = home.join(".codex/sessions/2026/09/21/import.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-09-21T08:00:00Z",
                    "payload": {
                        "id": "native-import-test",
                        "cwd": workspace,
                        "source": "test"
                    }
                })
            ),
        )
        .unwrap();
        let external = crate::external_sessions::discover_fresh(Some(&home), true)
            .unwrap()
            .sessions
            .into_iter()
            .find(|item| item.native_id == "native-import-test")
            .unwrap();
        assert!(external.process.is_none());

        let mut state = test_state_named(root.path(), "import-test");
        state.native_session_home = Some(home);
        let session = ClientSession::local(Some("person/tester")).unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/import-test".into(),
            action_type: "session.import".into(),
            idempotency_key: "session-import-test-0001".into(),
            fence: Fence {
                snapshot_id: new_client_snapshot(&state).id,
                subject_revisions: BTreeMap::from([(
                    external.id.clone(),
                    external.revision.clone(),
                )]),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: None,
                terminal_sequence: None,
                preview_token: None,
            },
            parameters: json!({"target_id": external.id}),
        };

        let affected = import_external_session_action(&state, &session, &request)
            .await
            .unwrap();
        assert_eq!(affected.len(), 2);
        assert!(affected[1].starts_with("agent/import/codex/"));
        assert!(state.store.active_mission_runs().unwrap().is_empty());
        let imported = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == affected[1])
            .expect("the imported durable seat is declared");
        assert_eq!(imported.kind, "agent");
        assert!(imported.owner_run.is_none());
        let session_file = state
            .store
            .latest_claim(&affected[1], Some("harness.session-file"))
            .unwrap()
            .expect("the native session identity is durable graph state");
        assert_eq!(
            session_file.body["fields"]["session_id"],
            "native-import-test"
        );
        assert_eq!(session_file.body["fields"]["harness"], "codex");
        assert_eq!(session_file.body["fields"]["agent"], affected[1]);
        assert_eq!(session_file.body["fields"]["source_session"], external.id);
        assert_eq!(
            session_file.body["fields"]["discovery_revision"],
            external.revision
        );
    }

    #[test]
    fn client_events_are_redacted_timestamped_invalidations_with_retention_fences() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "event-node");
        for record in [
            EventRecord {
                store_index: 7,
                kind: "custom.client.pairing-completed".into(),
                subject: "custom/client/pairing-secret".into(),
                body: json!({"fields": {"credential": "PAIRING-PLAINTEXT", "code": "123456"}}),
            },
            EventRecord {
                store_index: 8,
                kind: "custom.client.terminal-attached".into(),
                subject: "custom/client/terminal-secret".into(),
                body: json!({"fields": {"terminal_id": "terminal/agent/viewer", "capability": "TERMINAL-PLAINTEXT", "stream_url": "?secret=yes"}}),
            },
            EventRecord {
                store_index: 9,
                kind: "message.sent".into(),
                subject: "message/safe-id".into(),
                body: json!({"fields": {"content": "PRIVATE-MESSAGE", "from": "person/nathan", "to": "agent/worker", "session_id": "session/current"}}),
            },
        ] {
            let projected = safe_event_projection(&state, &record);
            let encoded = serde_json::to_string(&projected).unwrap();
            assert!(!encoded.contains("PLAINTEXT"));
            assert!(!encoded.contains("PRIVATE-MESSAGE"));
            assert!(!encoded.contains("stream_url"));
            assert!(!encoded.contains("credential"));
        }
        let message = safe_event_projection(
            &state,
            &EventRecord {
                store_index: 9,
                kind: "message.sent".into(),
                subject: "message/safe-id".into(),
                body: json!({"fields": {"session_id": "session/current"}}),
            },
        );
        assert_eq!(message.1, ["message/safe-id", "session/current"]);
        let pairing = safe_event_projection(
            &state,
            &EventRecord {
                store_index: 10,
                kind: "custom.client.pairing-revoked".into(),
                subject: "custom/client/pairing-device".into(),
                body: json!({"fields": {"device_id": "device/example"}}),
            },
        );
        assert_eq!(pairing.0, "capabilities.changed");
        for (index, kind) in [
            "custom.client.terminal-attached",
            "custom.client.terminal-consumed",
            "custom.client.terminal-detached",
        ]
        .into_iter()
        .enumerate()
        {
            let projected = safe_event_projection(
                &state,
                &EventRecord {
                    store_index: 11 + index as u64,
                    kind: kind.into(),
                    subject: "custom/client/terminal-attachment-viewer".into(),
                    body: json!({"fields": {
                        "terminal_id": "terminal/agent/viewer",
                        "attachment_id": "terminal-attachment/viewer"
                    }}),
                },
            );
            assert_eq!(projected.0, "upsert", "{kind}");
            assert_eq!(projected.1, ["terminal/agent/viewer"], "{kind}");
            assert_eq!(
                projected.2["reason"], "terminal-viewer-lifecycle-changed",
                "{kind}"
            );
        }
        let gap = validate_event_cursor("event-node", true, 10, 50, 90).unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        assert_eq!(gap.code, "cursor-gap");
        assert_eq!(gap.details["full_resync"], true);
        assert!(validate_event_cursor("event-node", true, 49, 50, 90).is_ok());

        let accepted = state
            .store
            .append_claim(&ClaimInput {
                subject: "message/original-time".into(),
                kind: "message.sent".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/nathan".into())),
                    ("to".into(), Value::String("agent/worker".into())),
                    ("content".into(), Value::String("safe".into())),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("event-original-time".into()),
            })
            .unwrap();
        let snapshot = client_snapshot_at(&state, accepted.store_index);
        assert_eq!(
            snapshot.created_at,
            client_timestamp(accepted.accepted_at_unix_ms)
        );
    }

    #[tokio::test]
    async fn capabilities_and_event_pages_publish_the_same_retained_cursor_floor() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "retention-node");
        let append = |id: &str, key: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/nathan".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("person/nathan".into())),
                        ("to".into(), Value::String("agent/worker".into())),
                        ("content".into(), Value::String("secret".into())),
                        ("status".into(), Value::String("sent".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap()
        };
        let first = append("old", "retention-old");
        let retained = append("retained", "retention-new");
        assert!(
            state
                .store
                .prune_events_before(retained.store_index)
                .unwrap()
                > 0
        );
        let floor = retained.store_index.saturating_sub(1);
        let session = ClientSession::local(None).unwrap();
        let snapshot = new_client_snapshot(&state);
        let capabilities = client_capabilities(
            State(state.clone()),
            Extension(snapshot),
            Extension(session.clone()),
        )
        .await
        .0;
        let expected = format!("event-cursor/retention-node/{floor}");
        assert_eq!(capabilities["oldest_event_cursor"], expected);

        let gap = events(
            State(state.clone()),
            Extension(session.clone()),
            Query(EventsQuery {
                after: Some(format!(
                    "event-cursor/retention-node/{}",
                    first.store_index.saturating_sub(1)
                )),
                limit: Some(10),
                wait_ms: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        let page = events(
            State(state),
            Extension(session),
            Query(EventsQuery {
                after: Some(expected.clone()),
                limit: Some(10),
                wait_ms: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(page["oldest_cursor"], expected);
        assert!(!serde_json::to_string(&page).unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn event_page_without_a_cursor_returns_the_latest_bounded_activity() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "activity-node");
        let mut accepted = Vec::new();
        for id in ["first", "second", "latest"] {
            accepted.push(
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: format!("message/{id}"),
                        kind: "message.sent".into(),
                        actor: Some("person/nathan".into()),
                        fields: BTreeMap::from([
                            ("from".into(), Value::String("person/nathan".into())),
                            ("to".into(), Value::String("agent/worker".into())),
                            ("content".into(), Value::String(id.into())),
                            ("status".into(), Value::String("sent".into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("activity-{id}")),
                    })
                    .unwrap(),
            );
        }
        let page = events(
            State(state),
            Extension(ClientSession::local(None).unwrap()),
            Query(EventsQuery {
                after: None,
                limit: Some(2),
                wait_ms: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["items"][0]["sequence"], accepted[1].store_index);
        assert_eq!(page["items"][1]["sequence"], accepted[2].store_index);
        assert_eq!(page["has_more"], false);
    }

    #[tokio::test]
    async fn client_event_long_poll_wakes_for_a_new_claim() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "wait-node");
        let waiter_state = state.clone();
        let waiter = tokio::spawn(async move {
            events(
                State(waiter_state),
                Extension(ClientSession::local(None).unwrap()),
                Query(EventsQuery {
                    after: Some("event-cursor/wait-node/0".into()),
                    limit: Some(10),
                    wait_ms: Some(1_000),
                }),
            )
            .await
        });
        tokio::task::yield_now().await;
        state
            .store
            .append_claim(&ClaimInput {
                subject: "message/wake".into(),
                kind: "message.sent".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/nathan".into())),
                    ("to".into(), Value::String("agent/worker".into())),
                    ("content".into(), Value::String("wake".into())),
                    ("status".into(), Value::String("sent".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("client-event-wake".into()),
            })
            .unwrap();
        signal_changed(&state);
        let page = tokio::time::timeout(Duration::from_millis(250), waiter)
            .await
            .expect("the client event long poll did not wake")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn current_agent_session_fences_composer_messages_and_timeline_history() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "session-message-node");
        let subject = "agent/session-message-owner";
        let incarnation = "session-message-runtime:i2";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("session-message-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("session-message-runtime".into()),
            })
            .unwrap();

        let snapshot = new_client_snapshot(&state);
        let agents = client_agent_resources(
            &state.store,
            false,
            &snapshot.created_at,
            snapshot.store_index,
        )
        .unwrap();
        let sessions = client_session_resources(
            &state.store,
            false,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap();
        let agent = agents.iter().find(|agent| agent["id"] == subject).unwrap();
        let owned_sessions = sessions
            .iter()
            .filter(|session| session["owner_id"] == subject)
            .collect::<Vec<_>>();
        assert_eq!(owned_sessions.len(), 1);
        let session_id = owned_sessions[0]["id"].as_str().unwrap().to_owned();
        assert_eq!(agent["current_session_id"], session_id);

        let client_session = ClientSession::local(Some("person/nathan")).unwrap();
        let action = |key: &str, parameters: Value| ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: format!("action/{key}"),
            action_type: "message.send".into(),
            idempotency_key: format!("session-message-{key}"),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: None,
                terminal_sequence: None,
                preview_token: None,
            },
            parameters,
        };

        let generic = action(
            "generic",
            json!({"to": subject, "content": "generic legacy message"}),
        );
        dispatch_action(&state, &snapshot, &client_session, &generic)
            .await
            .expect("generic messaging remains backward compatible");

        let current_snapshot = new_client_snapshot(&state);
        let composer = action(
            "composer",
            json!({
                "to": subject,
                "session_id": session_id,
                "content": "current composer message"
            }),
        );
        let affected = dispatch_action(&state, &current_snapshot, &client_session, &composer)
            .await
            .unwrap();
        let composer_claim = state
            .store
            .claims_for(&affected[0], Some("message.sent"))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            composer_claim.body["fields"]["session_id"],
            Value::String(session_id.clone())
        );
        let composer_resource = client_message_resources(&state.store, None, true, None)
            .unwrap()
            .into_iter()
            .find(|message| message["id"] == affected[0])
            .unwrap();
        assert_eq!(composer_resource["session_id"], session_id);

        let _ = accept_message(
            &state,
            MessageSendRequest {
                idempotency_key: "session-message-older".into(),
                from: client_session.authority_actor.clone(),
                to: subject.into(),
                content: "older session message".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
            },
            Some("session/older-incarnation".into()),
        )
        .unwrap();

        let rejected = action(
            "stale",
            json!({
                "to": subject,
                "session_id": "session/older-incarnation",
                "content": "must not be accepted"
            }),
        );
        let error = dispatch_action(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            &rejected,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "stale-fence");

        let timeline = timeline_value(
            &state,
            &new_client_snapshot(&state),
            &client_session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        let text = timeline["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["body"]["text"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(text, vec!["current composer message"]);
    }

    #[test]
    fn managed_codex_session_renders_its_exact_native_chat_not_only_status() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let transcript = home.join(".codex/sessions/2026/09/24/managed.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let owner = "agent/managed-codex";
        let incarnation = "native-pty:one";
        let provider_incarnation = "provider-one";
        let native_id = "native-managed-codex-test";
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type":"session_meta","timestamp":"2026-09-24T12:00:00Z","payload":{"id":native_id,"cwd":root.path(),"source":"test"}}),
                json!({"type":"response_item","timestamp":"2026-09-24T12:00:01Z","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"Exact managed transcript"}]}}),
            ),
        )
        .unwrap();
        let mut state = test_state_named(root.path(), "managed-codex-test");
        state.native_session_home = Some(home);
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("state");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("runtime.json"),
            serde_json::to_vec(
                &json!({"agent":"managed-codex","incarnation":provider_incarnation}),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            directory.join("binding.json"),
            serde_json::to_vec(&json!({"agent":"managed-codex","runtimeIncarnation":provider_incarnation,"threadId":native_id})).unwrap(),
        )
        .unwrap();
        for (kind, fields) in [
            (
                "runtime.observed",
                BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("managed-codex-pty".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
            ),
            (
                "harness.observed",
                BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "evidence_incarnation".into(),
                        Value::String(provider_incarnation.into()),
                    ),
                ]),
            ),
        ] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: owner.into(),
                    kind: kind.into(),
                    actor: Some(owner.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let snapshot = new_client_snapshot(&state);
        let session = ClientSession::local(Some("person/nathan")).unwrap();
        let session_id = super::managed_session_id(owner, incarnation);
        let timeline = timeline_value(
            &state,
            &snapshot,
            &session,
            &session_id,
            &ClientListQuery::default(),
        )
        .unwrap()
        .0;
        assert!(timeline["items"].as_array().unwrap().iter().any(|item| {
            item["type"] == "content" && item["body"]["text"] == "Exact managed transcript"
        }));
        std::fs::write(
            directory.join("binding.json"),
            serde_json::to_vec(
                &json!({"agent":"managed-codex","runtimeIncarnation":"other","threadId":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            managed_codex_transcript(&state, owner, incarnation)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_claude_session_uses_only_current_wrapper_binding() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let native_id = "11111111-1111-4111-8111-111111111111";
        let transcript = home.join(format!(".claude/projects/-test/{native_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!("{}\n", json!({"type":"assistant","sessionId":native_id,"timestamp":"2026-09-24T12:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Current Claude answer"}]}})),
        ).unwrap();
        let mut state = test_state_named(root.path(), "managed-claude-test");
        state.native_session_home = Some(home);
        let owner = "agent/managed-claude";
        let incarnation = "native-pty:current";
        let provider_incarnation = "provider-current";
        let identity = "managed-claude";
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("catalog/agents")
            .join(st2::run::detect_host())
            .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16]);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("claude-native-session"),
            serde_json::to_vec(
                &json!({"incarnation":provider_incarnation,"native_session_id":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.into(),
                kind: "harness.observed".into(),
                actor: Some(owner.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("claude".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "evidence_incarnation".into(),
                        Value::String(provider_incarnation.into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exact = super::managed_claude_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap();
        let timeline = crate::external_sessions::normalized_timeline(&exact).unwrap();
        assert!(
            timeline
                .iter()
                .any(|entry| entry["body"]["text"] == "Current Claude answer")
        );
        assert!(
            super::managed_claude_transcript(&state, owner, "native-pty:old")
                .unwrap()
                .is_none()
        );
        std::fs::write(
            directory.join("claude-native-session"),
            serde_json::to_vec(
                &json!({"incarnation":"provider-old","native_session_id":native_id}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            super::managed_claude_transcript(&state, owner, incarnation)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn managed_omp_session_reads_the_current_saved_conversation() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "managed-omp-test");
        let owner = "agent/fleet/pty-rust/omp";
        let identity = owner.strip_prefix("agent/").unwrap();
        let incarnation = "123:2026-09-25T15:11:54.870Z";
        let directory = state
            .state_dir
            .join("drivers")
            .join(&hex::encode(Sha256::digest(owner.as_bytes()))[..24])
            .join("catalog/agents")
            .join(st2::run::detect_host())
            .join(&hex::encode(Sha256::digest(identity.as_bytes()))[..16])
            .join("provider-sessions");
        std::fs::create_dir_all(&directory).unwrap();
        let old = directory.join("2026-09-25T14-00-00-000Z_old.jsonl");
        let current = directory.join("2026-09-25T15-11-55-793Z_current.jsonl");
        std::fs::write(
            &old,
            format!(
                "{}\n",
                json!({"type":"session","id":"old","timestamp":"2026-09-25T14:00:00Z","cwd":"/tmp"})
            ),
        )
        .unwrap();
        std::fs::write(
            &current,
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"session","id":"current","timestamp":"2026-09-25T15:11:55.793Z","cwd":"/tmp"}),
                json!({"type":"message","id":"answer","timestamp":"2026-09-25T15:12:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Saved OMP answer"},{"type":"toolCall","id":"call-1","name":"read","arguments":{"file":"example"}}]}}),
                json!({"type":"message","id":"result","timestamp":"2026-09-25T15:12:01Z","message":{"role":"toolResult","content":[{"type":"text","text":"{\"presence\":null}"}]}}),
            ),
        )
        .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: owner.into(),
                kind: "harness.observed".into(),
                actor: Some(owner.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("omp".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let exact = super::managed_omp_transcript(&state, owner, incarnation)
            .unwrap()
            .unwrap();
        assert_eq!(exact.native_id, "current");
        let timeline = crate::external_sessions::normalized_timeline(&exact).unwrap();
        assert!(
            timeline
                .iter()
                .any(|entry| entry["body"]["text"] == "Saved OMP answer")
        );
        assert!(timeline.iter().any(|entry| entry["type"] == "tool_call"));
        assert!(
            timeline
                .iter()
                .any(|entry| entry["role"] == "tool"
                    && entry["body"]["text"] == "{\"presence\":null}")
        );
        assert!(
            super::managed_omp_transcript(&state, owner, "123:2026-09-25T14:00:00Z")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn durable_timeline_is_cursor_paged_and_enforces_replace_finalize_identity() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "timeline-node");
        let subject = "agent/timeline-owner";
        let incarnation = "timeline-runtime:i1";
        let append = |kind: &str, fields: BTreeMap<String, Value>, key: &str| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: kind.into(),
                    actor: Some(subject.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        };
        append(
            "runtime.observed",
            BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                (
                    "runtime_id".into(),
                    Value::String("timeline-runtime".into()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("terminal".into(), Value::Bool(false)),
            ]),
            "timeline-runtime",
        );
        let message = state
            .store
            .append_claim(&ClaimInput {
                subject: "message/timeline-user".into(),
                kind: "message.sent".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("from".into(), Value::String("person/nathan".into())),
                    ("to".into(), Value::String(subject.into())),
                    ("content".into(), Value::String("do the work".into())),
                    ("status".into(), Value::String("sent".into())),
                    (
                        "session_id".into(),
                        Value::String(managed_session_id(subject, incarnation)),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("timeline-message".into()),
            })
            .unwrap();
        assert_eq!(message.kind, "message.sent");
        let timeline = |operation: &str,
                        entry_id: &str,
                        revision: u64,
                        role: &str,
                        entry_type: &str,
                        final_entry: bool,
                        body: Value| {
            BTreeMap::from([
                ("operation".into(), Value::String(operation.into())),
                ("entry_id".into(), Value::String(entry_id.into())),
                ("revision".into(), Value::from(revision)),
                ("role".into(), Value::String(role.into())),
                ("entry_type".into(), Value::String(entry_type.into())),
                ("final".into(), Value::Bool(final_entry)),
                ("body".into(), body),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ])
        };
        append(
            "harness.timeline",
            timeline(
                "append",
                "timeline-entry/provider-content",
                1,
                "assistant",
                "content",
                false,
                json!({"media_type":"text/plain", "text":"draft"}),
            ),
            "timeline-content-append",
        );
        append(
            "harness.timeline",
            timeline(
                "replace",
                "timeline-entry/provider-content",
                2,
                "assistant",
                "content",
                false,
                json!({"media_type":"text/plain", "text":"revised"}),
            ),
            "timeline-content-replace",
        );
        append(
            "harness.timeline",
            timeline(
                "finalize",
                "timeline-entry/provider-content",
                3,
                "assistant",
                "content",
                true,
                json!({"media_type":"text/plain", "text":"final"}),
            ),
            "timeline-content-finalize",
        );
        let mut wrong_incarnation = timeline(
            "append",
            "timeline-entry/wrong-incarnation",
            1,
            "assistant",
            "content",
            true,
            json!({"media_type":"text/plain", "text":"must not cross incarnations"}),
        );
        wrong_incarnation.insert(
            "incarnation_id".into(),
            Value::String("timeline-runtime:old".into()),
        );
        append(
            "harness.timeline",
            wrong_incarnation,
            "timeline-wrong-incarnation",
        );
        let mut missing_incarnation = timeline(
            "append",
            "timeline-entry/missing-incarnation",
            1,
            "assistant",
            "content",
            true,
            json!({"media_type":"text/plain", "text":"must be rejected"}),
        );
        missing_incarnation.remove("incarnation_id");
        let missing = state.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "harness.timeline".into(),
            actor: Some(subject.into()),
            fields: missing_incarnation,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("timeline-missing-incarnation".into()),
        });
        assert_eq!(missing.unwrap_err().code, "missing-claim-field");
        for (entry_id, role, entry_type, body) in [
            (
                "timeline-entry/tool-call",
                "assistant",
                "tool_call",
                json!({"call_id":"call/1", "name":"shell", "arguments":{"command":"true"}}),
            ),
            (
                "timeline-entry/tool-result",
                "tool",
                "tool_result",
                json!({"call_id":"call/1", "status":"success", "media_type":"text/plain", "content":"ok"}),
            ),
            (
                "timeline-entry/redaction",
                "system",
                "redaction",
                json!({"reason":"credential", "withheld_bytes":12}),
            ),
            (
                "timeline-entry/truncation",
                "system",
                "truncation",
                json!({"reason":"limit", "omitted_from_sequence":90, "omitted_to_sequence":99}),
            ),
            (
                "timeline-entry/usage-without-source-semantics",
                "system",
                "usage",
                json!({"total_tokens":7}),
            ),
        ] {
            append(
                "harness.timeline",
                timeline("append", entry_id, 1, role, entry_type, true, body),
                &format!(
                    "timeline-{}",
                    entry_id.trim_start_matches("timeline-entry/")
                ),
            );
        }
        append(
            "harness.diagnostic",
            BTreeMap::from([
                ("severity".into(), Value::String("warning".into())),
                ("code".into(), Value::String("provider-warning".into())),
                ("reason".into(), Value::String("retry later".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            "timeline-diagnostic",
        );
        append(
            "harness.usage",
            BTreeMap::from([
                (
                    "semantics".into(),
                    Value::String("session_cumulative".into()),
                ),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("total_tokens".into(), Value::from(42)),
            ]),
            "timeline-usage",
        );

        let snapshot = new_client_snapshot(&state);
        let session_id = client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let client_session = ClientSession::local(None).unwrap();
        let mut cursor = None;
        let mut entries = Vec::new();
        let mut wrote_during_pagination = false;
        loop {
            let query = ClientListQuery {
                limit: Some(3),
                cursor: cursor.clone(),
                ..ClientListQuery::default()
            };
            let page = timeline_value(
                &state,
                &snapshot,
                &client_session,
                session_id.trim_start_matches("session/"),
                &query,
            )
            .unwrap()
            .0;
            let page_items = page["items"].as_array().unwrap().iter().cloned();
            entries.splice(0..0, page_items);
            cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_some() && !wrote_during_pagination {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: "agent/unrelated-timeline-writer".into(),
                        kind: "runtime.observed".into(),
                        actor: Some("agent/unrelated-timeline-writer".into()),
                        fields: BTreeMap::from([
                            (
                                "runtime_id".into(),
                                Value::String("unrelated-runtime".into()),
                            ),
                            (
                                "incarnation_id".into(),
                                Value::String("unrelated-runtime:i1".into()),
                            ),
                            ("status".into(), Value::String("running".into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
                wrote_during_pagination = true;
            }
            if cursor.is_none() {
                break;
            }
        }
        let types = entries
            .iter()
            .filter_map(|entry| entry["type"].as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            types,
            BTreeSet::from([
                "message",
                "content",
                "tool_call",
                "tool_result",
                "status",
                "error",
                "usage",
                "redaction",
                "truncation",
            ])
        );
        let final_content = entries
            .iter()
            .find(|entry| entry["id"] == "timeline-entry/provider-content")
            .unwrap();
        assert_eq!(final_content["revision"], 3);
        assert_eq!(final_content["final"], true);
        assert_eq!(final_content["body"]["text"], "final");
        let inferred_usage = entries
            .iter()
            .find(|entry| entry["id"] == "timeline-entry/usage-without-source-semantics")
            .unwrap();
        assert_eq!(inferred_usage["body"]["semantics"], "response");
        assert_eq!(inferred_usage["body"]["driver"], "codex");
        assert_eq!(inferred_usage["body"]["attribution"]["agent_id"], subject);
        assert!(
            entries
                .iter()
                .all(|entry| entry["id"] != "timeline-entry/wrong-incarnation"),
            "explicit timeline claims are fenced to the live incarnation"
        );
        assert!(entries.windows(2).all(|pair| {
            pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap()
        }));
    }

    #[test]
    fn timeline_retention_requires_an_actual_typed_gap_interval() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state_named(root.path(), "timeline-retention-node");
        let subject = "agent/timeline-retention-owner";
        let incarnation = "timeline-retention-runtime:i1";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    (
                        "runtime_id".into(),
                        Value::String("timeline-retention-runtime".into()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("terminal".into(), Value::Bool(false)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("timeline-retention-runtime".into()),
            })
            .unwrap();
        let append_entry = |sequence: u64, entry_type: &str, body: Value| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "harness.timeline".into(),
                    actor: Some(subject.into()),
                    fields: BTreeMap::from([
                        ("operation".into(), Value::String("append".into())),
                        (
                            "entry_id".into(),
                            Value::String(format!("timeline-entry/retention-{sequence}")),
                        ),
                        ("sequence".into(), Value::from(sequence)),
                        ("revision".into(), Value::from(1)),
                        ("role".into(), Value::String("system".into())),
                        ("entry_type".into(), Value::String(entry_type.into())),
                        ("final".into(), Value::Bool(true)),
                        ("body".into(), body),
                        ("driver".into(), Value::String("codex".into())),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("timeline-retention-{sequence}")),
                })
                .unwrap();
        };
        for sequence in 1..=4_097 {
            append_entry(
                sequence,
                "status",
                json!({"status":"running", "detail":format!("event {sequence}")}),
            );
        }
        let session = ClientSession::local(None).unwrap();
        let snapshot = new_client_snapshot(&state);
        let session_id = client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let gap = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap_err();
        assert_eq!(gap.status, StatusCode::GONE);
        assert_eq!(gap.code, "cursor-gap");
        assert_eq!(gap.details.get("full_resync"), Some(&Value::Bool(true)));

        append_entry(
            4_098,
            "truncation",
            json!({
                "reason":"producer-retention",
                "omitted_from_sequence":1,
                "omitted_to_sequence":1
            }),
        );
        let snapshot = new_client_snapshot(&state);
        let insufficient = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .unwrap_err();
        assert_eq!(insufficient.code, "cursor-gap");

        append_entry(
            4_099,
            "truncation",
            json!({
                "reason":"producer-retention",
                "omitted_from_sequence":1,
                "omitted_to_sequence":3
            }),
        );
        let snapshot = new_client_snapshot(&state);
        let _ = timeline_value(
            &state,
            &snapshot,
            &session,
            session_id.trim_start_matches("session/"),
            &ClientListQuery::default(),
        )
        .expect("the typed retention intervals cover the complete omitted logical prefix");
    }

    #[test]
    fn usage_attribution_and_rollups_follow_exact_step_and_mission_ownership() {
        let store = Store::open_memory("usage-attribution-node").unwrap();
        let desired = [
            crate::model::DesiredSubject {
                subject: "agent/usage-a".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/one".into()),
                owner_generation: Some("run-generation/gen-one".into()),
                owner_step: Some("step-run/gen-one/step-a".into()),
            },
            crate::model::DesiredSubject {
                subject: "agent/usage-b".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/one".into()),
                owner_generation: Some("run-generation/gen-one".into()),
                owner_step: Some("step-run/gen-one/step-b".into()),
            },
            crate::model::DesiredSubject {
                subject: "agent/usage-other".into(),
                kind: "agent".into(),
                desired: json!({}),
                member: None,
                owner_run: Some("mission-run/example/two".into()),
                owner_generation: Some("run-generation/gen-two".into()),
                owner_step: Some("step-run/gen-two/step-c".into()),
            },
        ];
        for (subject, total) in [
            ("agent/usage-a", 10_u64),
            ("agent/usage-b", 20),
            ("agent/usage-other", 99),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "harness.usage".into(),
                    actor: Some(subject.into()),
                    fields: BTreeMap::from([
                        (
                            "semantics".into(),
                            Value::String("session_cumulative".into()),
                        ),
                        ("driver".into(), Value::String("codex".into())),
                        (
                            "incarnation_id".into(),
                            Value::String(format!("{subject}:i1")),
                        ),
                        ("total_tokens".into(), Value::from(total)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("usage-{total}")),
                })
                .unwrap();
        }
        let attribution = timeline_attribution("agent/usage-a", &desired);
        assert_eq!(attribution["agent_id"], "agent/usage-a");
        assert_eq!(attribution["mission_run_id"], "mission-run/example/one");
        assert_eq!(attribution["generation_id"], "run-generation/gen-one");
        assert_eq!(attribution["step_id"], "step-run/gen-one/step-a");

        let step = aggregate_usage_for_step(&store, &desired, "step-run/gen-one/step-a", None)
            .unwrap()
            .unwrap();
        assert_eq!(step.total_tokens, 10);
        let mission = aggregate_usage_for_runs(
            &store,
            &desired,
            &BTreeSet::from(["mission-run/example/one"]),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(mission.total_tokens, 30);
        assert_eq!(mission.incarnation_count, 2);
    }

    #[test]
    fn terminal_attach_reconciles_a_pre_receipt_restart_without_secret_at_rest() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/terminal-owner".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/terminal-owner".into()),
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        Value::String("terminal-runtime".into()),
                    ),
                    (
                        "incarnation_id".into(),
                        Value::String("terminal-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let session = ClientSession::local(Some("person/nathan")).unwrap();
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/terminal-attach-crash".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "terminal-attach-crash-0001".into(),
            fence: Fence {
                snapshot_id: "snapshot/test".into(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("terminal-runtime:i1".into()),
                terminal_sequence: Some(1),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/terminal-owner" }),
        };
        let first = create_terminal_attachment(&state, &session, &request, "request-digest")
            .expect("pre-receipt attachment");
        let capability = first["stream_capability"].as_str().unwrap().to_owned();
        let attachment_id = first["attachment_id"].as_str().unwrap().to_owned();
        let stored = serde_json::to_string(
            &state
                .store
                .claims_page(None, None, 0, None, true, 100)
                .unwrap(),
        )
        .unwrap();
        assert!(!stored.contains(&capability));
        assert!(!stored.contains("capability="));
        assert_eq!(stored.matches("custom.client.terminal-attached").count(), 1);
        let key_metadata = fs::metadata(root.path().join("client-terminal.key")).unwrap();
        assert_eq!(key_metadata.mode() & 0o777, 0o600);
        assert_eq!(key_metadata.uid(), unsafe { libc::geteuid() });

        drop(state);
        let restarted = test_state(root.path());
        let reconciled =
            create_terminal_attachment(&restarted, &session, &request, "request-digest")
                .expect("retry reconciles the pre-receipt attachment");
        assert_eq!(reconciled["attachment_id"], attachment_id);
        assert_eq!(reconciled["stream_capability"], capability);
        let stored = restarted
            .store
            .claims_page(None, None, 0, None, true, 100)
            .unwrap();
        assert_eq!(
            stored
                .claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.terminal-attached")
                .count(),
            1
        );

        consume_terminal_attachment(
            &restarted,
            &session,
            "terminal/agent/terminal-owner",
            "terminal-runtime:i1",
            Some(&capability),
        )
        .unwrap();
        let consumed = terminal_attachment_response(&restarted, &session, &attachment_id).unwrap();
        assert_eq!(consumed["state"], "consumed");
        assert!(consumed["stream_capability"].is_null());
        let detach = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/terminal-detach-after-consume".into(),
            action_type: "terminal.detach".into(),
            idempotency_key: "terminal-detach-after-consume-0001".into(),
            fence: Fence {
                snapshot_id: "snapshot/test".into(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("terminal-runtime:i1".into()),
                terminal_sequence: None,
                preview_token: None,
            },
            parameters: json!({ "target_id": attachment_id }),
        };
        detach_terminal_attachment(&restarted, &session, &detach).unwrap();
        let lifecycle = restarted
            .store
            .claims_for(
                &terminal_attachment_subject(consumed["attachment_id"].as_str().unwrap()).unwrap(),
                None,
            )
            .unwrap()
            .into_iter()
            .filter(|claim| claim.kind.starts_with("custom.client.terminal-"))
            .collect::<Vec<_>>();
        assert_eq!(lifecycle.len(), 3);
        for claim in lifecycle {
            let projected = safe_event_projection(
                &restarted,
                &EventRecord {
                    store_index: claim.store_index,
                    kind: claim.kind.clone(),
                    subject: claim.subject.clone(),
                    body: claim.body.clone(),
                },
            );
            assert_eq!(projected.0, "upsert", "{}", claim.kind);
            assert_eq!(
                projected.1,
                ["terminal/agent/terminal-owner"],
                "{}",
                claim.kind
            );
        }
    }

    #[test]
    fn concurrent_same_key_attach_publishes_one_attachment_and_one_sanitized_receipt() {
        let root = tempfile::tempdir().unwrap();
        let state = test_state(root.path());
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/concurrent-terminal-owner".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/concurrent-terminal-owner".into()),
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        Value::String("concurrent-terminal-runtime".into()),
                    ),
                    (
                        "incarnation_id".into(),
                        Value::String("concurrent-terminal-runtime:i1".into()),
                    ),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let snapshot = new_client_snapshot(&state);
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/concurrent-terminal-attach".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "concurrent-terminal-attach-key-0001".into(),
            fence: Fence {
                snapshot_id: snapshot.id.clone(),
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("concurrent-terminal-runtime:i1".into()),
                terminal_sequence: Some(snapshot.store_index),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/concurrent-terminal-owner" }),
        };
        let session = ClientSession::local(Some("person/nathan")).unwrap();
        let count = 8;
        let barrier = Arc::new(Barrier::new(count));
        let threads = (0..count)
            .map(|_| {
                let state = state.clone();
                let snapshot = snapshot.clone();
                let request = request.clone();
                let session = session.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(action(
                            State(state),
                            Extension(snapshot),
                            Extension(session),
                            Json(request),
                        ))
                        .map(|Json(result)| result)
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let attachment_id = results[0]["terminal_attachment"]["attachment_id"]
            .as_str()
            .unwrap();
        let capability = results[0]["terminal_attachment"]["stream_capability"]
            .as_str()
            .unwrap();
        assert!(results.iter().all(|result| {
            result["terminal_attachment"]["attachment_id"] == attachment_id
                && result["terminal_attachment"]["stream_capability"] == capability
        }));

        let page = state
            .store
            .claims_page(None, None, 0, None, true, 100)
            .unwrap();
        assert_eq!(
            page.claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.terminal-attached")
                .count(),
            1
        );
        assert_eq!(
            page.claims
                .iter()
                .filter(|claim| claim.kind == "custom.client.action-result")
                .count(),
            1
        );
        let stored = serde_json::to_string(&page).unwrap();
        assert!(!stored.contains(capability));
        assert!(!stored.contains("st3.cap."));
        assert!(!stored.contains("stream_capability"));
    }

    #[test]
    fn terminal_capabilities_and_pty_reads_are_bound_to_the_selected_owner_host() {
        let owner_root = tempfile::tempdir().unwrap();
        let follower_root = tempfile::tempdir().unwrap();
        let owner = test_state_named(owner_root.path(), "owner-node");
        let mut follower = test_state_named(follower_root.path(), "follower-node");
        let subject = "agent/fleet-terminal";
        let runtime = |status: &str, incarnation: &str, key: &str| ClaimInput {
            subject: subject.into(),
            kind: "runtime.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("runtime_id".into(), Value::String("same-runtime-id".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
                ("status".into(), Value::String(status.into())),
                ("terminal".into(), Value::Bool(true)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        };
        owner
            .store
            .append_claim(&runtime("running", "same-runtime-id:i1", "owner-running"))
            .unwrap();
        let session = ClientSession::local(Some("person/nathan")).unwrap();
        let projected = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(projected["state"], "running");
        assert_eq!(projected["owner_host_id"], "host/owner-node");
        assert_eq!(projected["terminal_access"]["read"], "granted");
        let request = ActionRequest {
            api_version: CLIENT_API_VERSION.into(),
            id: "action/fleet-terminal-attach".into(),
            action_type: "terminal.attach".into(),
            idempotency_key: "fleet-terminal-attach-0001".into(),
            fence: Fence {
                snapshot_id: new_client_snapshot(&owner).id,
                subject_revisions: BTreeMap::new(),
                mission_generation: None,
                step_definition: None,
                attempt: None,
                readiness_epoch: None,
                runtime_incarnation: Some("same-runtime-id:i1".into()),
                terminal_sequence: Some(owner.store.index().unwrap()),
                preview_token: None,
            },
            parameters: json!({ "target_id": "terminal/agent/fleet-terminal" }),
        };
        let attached =
            create_terminal_attachment(&owner, &session, &request, "fleet-request-digest").unwrap();
        assert_eq!(attached["owner_host_id"], "host/owner-node");
        let attachment_id = attached["attachment_id"].as_str().unwrap();
        let capability = attached["stream_capability"].as_str().unwrap();
        let owner_replay =
            create_terminal_attachment(&owner, &session, &request, "fleet-request-digest").unwrap();
        assert_eq!(owner_replay["stream_capability"], capability);

        follower
            .store
            .import_replication("owner-node", &owner.store.export_replication(0).unwrap())
            .unwrap();
        let error = terminal_attachment_response(&follower, &session, attachment_id).unwrap_err();
        assert_eq!(error.code, "forbidden");
        let error = consume_terminal_attachment(
            &follower,
            &session,
            "terminal/agent/fleet-terminal",
            "same-runtime-id:i1",
            Some(capability),
        )
        .unwrap_err();
        assert_eq!(error.code, "forbidden");
        let error =
            terminal_screen_value(&follower, subject, Some("same-runtime-id:i1")).unwrap_err();
        assert_eq!(error.code, "runtime-not-local");

        let secret = follower_root.path().join("fleet-secret");
        std::fs::write(&secret, [7_u8; 32]).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        follower.client_relay = crate::peer::ClientRelay::from_config(&crate::config::Config {
            node: "follower-node".into(),
            fleet_id: Some("fleet-test".into()),
            shared_secret_file: Some(secret),
            peers: vec![crate::config::PeerConfig {
                name: "owner-node".into(),
                url: "http://127.0.0.1:9".into(),
            }],
            ..Default::default()
        })
        .unwrap();
        let paired = ClientSession {
            actor: "person/nathan/session/device-one".into(),
            authority_actor: "person/nathan".into(),
            transport: "paired",
            scopes: ["terminal.read".into()].into_iter().collect(),
        };
        let mut remote_request = request.clone();
        remote_request.idempotency_key = "fleet-terminal-attach-device-one".into();
        let remote =
            create_terminal_attachment(&follower, &paired, &remote_request, "remote-digest")
                .unwrap();
        assert_eq!(remote["owner_host_id"], "host/owner-node");
        let remote_capability = remote["stream_capability"].as_str().unwrap();
        assert_ne!(remote_capability, capability);
        let another_device = ClientSession {
            actor: "person/nathan/session/device-two".into(),
            ..paired.clone()
        };
        assert_eq!(
            consume_terminal_attachment(
                &follower,
                &another_device,
                "terminal/agent/fleet-terminal",
                "same-runtime-id:i1",
                Some(remote_capability)
            )
            .unwrap_err()
            .code,
            "forbidden"
        );
        assert_eq!(
            consume_terminal_attachment(
                &follower,
                &paired,
                "terminal/agent/fleet-terminal",
                "same-runtime-id:i1",
                Some(capability)
            )
            .unwrap_err()
            .code,
            "forbidden"
        );
        consume_terminal_attachment(
            &follower,
            &paired,
            "terminal/agent/fleet-terminal",
            "same-runtime-id:i1",
            Some(remote_capability),
        )
        .unwrap();
        assert_eq!(
            terminal_attachment_response(
                &follower,
                &paired,
                remote["attachment_id"].as_str().unwrap()
            )
            .unwrap()["state"],
            "consumed"
        );

        owner
            .store
            .append_claim(&runtime("exited", "same-runtime-id:i1", "owner-exited"))
            .unwrap();
        let error = terminal_live_session(&owner, subject, Some("same-runtime-id:i1")).unwrap_err();
        assert_eq!(error.code, "not-found");
        let exited = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(exited["state"], "exited");
        assert_eq!(exited["terminal_access"]["read"], "unavailable");

        let stale_root = tempfile::tempdir().unwrap();
        let stale = test_state_named(stale_root.path(), "stale-node");
        stale
            .store
            .append_claim(&runtime("running", "same-runtime-id:i1", "stale-running"))
            .unwrap();
        owner
            .store
            .import_replication("stale-node", &stale.store.export_replication(0).unwrap())
            .unwrap();
        let status = owner.store.status(Some(subject)).unwrap();
        assert_eq!(status.subjects[0].reachability, "indeterminate");
        assert!(status.subjects[0].actual_origin.is_some());
        let error = terminal_live_session(&owner, subject, Some("same-runtime-id:i1")).unwrap_err();
        assert_eq!(error.code, "runtime-authority-indeterminate");
        let indeterminate = runtime_resources(&owner, true, &new_client_snapshot(&owner), &session)
            .unwrap()
            .into_iter()
            .find(|runtime| runtime["owner_id"] == subject)
            .unwrap();
        assert_eq!(indeterminate["state"], "unreachable");
        assert_eq!(indeterminate["terminal_access"]["read"], "unavailable");
        assert_eq!(indeterminate["operational"]["actionable"], false);
    }
}
