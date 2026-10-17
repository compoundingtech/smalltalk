use super::*;

#[derive(Deserialize)]
pub struct AgentRolloutRequest {
    subject: String,
    actor: String,
    expected_desired: String,
    expected_incarnation: String,
    policy: crate::rollout::Policy,
    idempotency_key: String,
}

pub async fn rollout_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRolloutRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-rollout-actor")?;
    let subject = agent_subject(request.subject);
    let old =
        crate::rollout::launched_member(&state.store, &subject, &request.expected_incarnation)
            .map_err(ApiError::internal)?
            .map(|(_, member)| member)
            .or_else(|| {
                state
                    .store
                    .rollout(&subject)
                    .ok()
                    .flatten()
                    .filter(|o| o.old_incarnation == request.expected_incarnation)
                    .map(|o| o.old_member)
            })
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "rollout-launch-unknown",
                    "the named incarnation has no original launch receipt",
                ))
            })?;
    let response = state
        .store
        .request_rollout(
            &subject,
            &request.expected_desired,
            &old,
            &request.expected_incarnation,
            &actor,
            &request.policy,
            &format!("seat-rollout-retry:{subject}:{}", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

#[derive(Deserialize)]
pub struct AgentRestartRequest {
    subject: String,
    actor: String,
    idempotency_key: String,
    /// Restart without first asking the seat for its handoff.
    #[serde(default)]
    now: bool,
}

pub async fn restart_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRestartRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-restart-actor")?;
    let subject = if request.subject.starts_with("agent/") {
        request.subject
    } else {
        format!("agent/{}", request.subject)
    };
    state.store.owned_member_guard(&subject).map_err(ApiError::bad)?;
    let key = format!("agent-restart:{subject}:{}", request.idempotency_key);
    if let Some(prior) = state
        .store
        .operation_claim(&key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(prior));
    }
    if state
        .store
        .rollout(&subject)
        .map_err(ApiError::bad)?
        .is_some_and(|o| o.holds_seat())
        || crate::rollout::status(&state.store, &subject)
            .map_err(ApiError::internal)?
            .is_some_and(|s| s["mode"] == "manual" && s["phase"] == "pending")
    {
        return Err(ApiError::bad(St3Error::new(
            "rollout-in-progress",
            "the seat has a rollout; inspect it or retry with st agents rollout",
        )));
    }
    if crate::suspension::current(&state.store, &subject)
        .map_err(ApiError::internal)?
        .is_some_and(|suspension| suspension.holds_seat())
    {
        return Err(ApiError::bad(St3Error::new(
            "restart-suspended",
            "the seat is suspended; resume it with `st agents resume`. If resume refuses an authored session selection, use `st agents stop` then `st agents start`; this ends the suspension and launches the declaration without preserving the suspended session",
        )));
    }
    let status = state
        .store
        .status(Some(&subject))
        .map_err(ApiError::internal)?;
    let current = status
        .subjects
        .iter()
        .find(|item| item.subject == subject)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-agent",
                format!("no seat `{subject}`"),
            ))
        })?;
    if !current.conflicts.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "restart-conflict",
            "resolve the seat's conflicting declarations before restarting",
        )));
    }
    let desired = state
        .store
        .desired_subject_with_writer(&subject)
        .map_err(ApiError::internal)?
        .map(|(desired, _)| desired)
        .filter(|desired| desired.kind == "agent")
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "restart-not-declared",
                "restart needs an active seat declaration; start a stopped seat first",
            ))
        })?;
    let member = desired
        .member
        .as_ref()
        .filter(|member| member.lifecycle == crate::model::MemberLifecycle::Service)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "restart-no-launch",
                "the seat has no readable service launch declaration",
            ))
        })?;
    let token = current.desired_token.clone().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "restart-not-declared",
            "the seat has no selected declaration",
        ))
    })?;
    let incarnation = current
        .actual
        .as_ref()
        .map(|actual| actual.get("fields").unwrap_or(actual))
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "runtime.action.requested".into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("action".into(), Value::String("restart".into())),
                (
                    "runtime_id".into(),
                    Value::String(member.runtime_id.clone()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ])
            .into_iter()
            .chain(request.now.then(|| {
                (
                    "operation".to_owned(),
                    Value::String(crate::handoff::RESTART_NOW_OPERATION.into()),
                )
            }))
            .collect(),
            evidence: vec![token],
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
pub struct MissionSeatStartRequest {
    subject: String,
    actor: String,
    /// The selected stop the caller read, when it read one.
    #[serde(default)]
    expected: Option<String>,
    idempotency_key: String,
}

/// Record an operator's source-offline exception for one exact placement.
pub async fn override_placement_source(
    State(state): State<AppState>,
    Json(mut request): Json<crate::placement::SourceOfflineRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request.actor = person_or_agent_actor(&request.actor, "invalid-source-offline-actor")?;
    request.subject = agent_subject(request.subject);
    let store = state.store.clone();
    let (claim, appended) = blocking_action(move || {
        let input = crate::placement::source_offline_input(&store, request)?;
        store.append_claim_outcome(&input)
    }).await?;
    if appended { signal_claim_changed(&state, crate::placement::SOURCE_OFFLINE_KIND); }
    Ok(Json(claim))
}

/// Start a stopped mission seat on the declaration its run gave it, as `st agents start` does.
pub async fn start_mission_seat(
    State(state): State<AppState>,
    Json(request): Json<MissionSeatStartRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-start-actor")?;
    let subject = agent_subject(request.subject);
    let response = state
        .store
        .start_mission_seat(
            &subject,
            request.expected.as_deref(),
            &actor,
            &format!("agent-start:{subject}:{}", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

/// Ask the seat's owner to suspend it. The seat must be running and quiet now: its driver reports
/// the harness quiescent, it holds no claimed step and runs no subagent, and its driver has bound
/// a native session to resume. The owner checks again before it stops anything.
pub async fn suspend_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentSuspensionRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request_suspend(&state, request, None).map(Json)
}

#[derive(Deserialize, Serialize)]
pub struct SnapshotReadRequest {
    pub subject: String,
    pub suspend_operation: String,
    pub resume_operation: String,
    pub actor: String,
    pub offset: u64,
}

pub async fn seat_snapshot_chunk(
    State(state): State<AppState>,
    Json(request): Json<SnapshotReadRequest>,
) -> Result<Json<Value>, ApiError> {
    person_or_agent_actor(&request.actor, "invalid-resume-actor")?;
    let suspension = crate::suspension::current(&state.store, &request.subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("no suspension"))?;
    if suspension.operation_id != request.resume_operation
        || suspension.suspend_operation_id.as_deref() != Some(&request.suspend_operation)
        || suspension.source_host.as_deref() != Some(&state.node)
        || suspension.requested_by.as_deref() != Some(&request.actor)
        || !matches!(suspension.phase.as_str(), "fencing-source" | "transferring")
    {
        return Err(ApiError::bad(St3Error::new(
            "stale-fence",
            "snapshot read does not name the current move",
        )));
    }
    let completed = state
        .store
        .operation_claim(&crate::suspension::suspend_completed_key(
            &request.suspend_operation,
        ))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("source is not suspended"))?;
    if completed.origin != state.node {
        return Err(ApiError::bad(St3Error::new(
            "stale-fence",
            "snapshot belongs to another source",
        )));
    }
    let sources = crate::placement::latest_by_origin(&state.store, &request.subject, u64::MAX)
        .map_err(ApiError::internal)?;
    if !sources
        .get(&state.node)
        .is_some_and(|claim| crate::placement::field(claim, "status") == Some("stopped"))
    {
        return Err(ApiError::bad(St3Error::new(
            "stale-fence",
            "source runtime has not acknowledged its stop",
        )));
    }
    crate::seat_snapshot::chunk(&state.state_dir, &request.suspend_operation, request.offset)
        .map(Json)
        .map_err(ApiError::internal)
}

/// Ask the seat's owner to resume a suspended seat on the native session it suspended.
pub async fn resume_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentSuspensionRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request_resume(&state, request, None).map(Json)
}

#[derive(Deserialize)]
pub struct NativeSessionReport {
    #[serde(default)]
    account_ref: Option<String>,
    subject: String,
    actor: String,
    incarnation_id: String,
    harness: String,
    session_id: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    desired_token: Option<String>,
}

/// A seat's driver reports the native session its harness bound for one incarnation. Only the
/// seat itself reports its own session.
pub async fn report_native_session(
    State(state): State<AppState>,
    Json(request): Json<NativeSessionReport>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let subject = agent_subject(request.subject);
    if request.actor != subject {
        return Err(ApiError::bad(St3Error::new(
            "foreign-agent-actor",
            "a seat reports only its own native session",
        )));
    }
    if request.session_id.is_empty() || request.incarnation_id.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-native-session",
            "a native session report needs a session ID and an incarnation",
        )));
    }
    let mut fields = BTreeMap::from([
        ("harness".into(), Value::String(request.harness)),
        (
            "session_id".into(),
            Value::String(request.session_id.clone()),
        ),
        ("agent".into(), Value::String(subject.clone())),
        (
            "incarnation_id".into(),
            Value::String(request.incarnation_id.clone()),
        ),
        ("status".into(), Value::String("active".into())),
    ]);
    if let Some(path) = request.path {
        fields.insert("path".into(), Value::String(path));
    }
    if let Some(account) = request.account_ref.filter(|name| !name.is_empty()) {
        fields.insert("account_ref".into(), Value::String(account));
    }
    if let Some(token) = request.desired_token.filter(|token| !token.is_empty()) {
        fields.insert("desired_token".into(), Value::String(token));
    }
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.session-file".into(),
            actor: Some(subject.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "native-session:{subject}:{}:{}",
                request.incarnation_id, request.session_id
            )),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
pub struct AgentRenameRequest {
    subject: String,
    name: Option<String>,
    actor: String,
    idempotency_key: String,
}

pub async fn rename_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRenameRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let subject = if request.subject.starts_with("agent/") {
        request.subject
    } else {
        format!("agent/{}", request.subject)
    };
    if !request.actor.starts_with("person/") {
        normalized_agent_actor(&request.actor).ok_or_else(|| {
            ApiError::bad(St3Error::new("invalid-rename-actor", "rename needs a person or agent actor"))
        })?;
    }
    let response = state.store.rename_agent(
        &subject,
        request.name.as_deref(),
        &request.idempotency_key,
    ).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}
