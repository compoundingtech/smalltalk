use super::*;

/// Only a locally bound harness may recover a missing work fence from its own runtime.
/// Explicit fences are retained so the action's validation can refuse stale callers.
fn bind_work_incarnation(
    state: &AppState,
    bound: Option<&Extension<BoundAgent>>,
    actor: Option<&str>,
    incarnation: &mut Option<String>,
) -> Result<Option<String>, ApiError> {
    let Some(bound) = bound else { return Ok(None) };
    let seat = &bound.0.0;
    if actor.and_then(normalized_agent_actor).as_deref() != Some(seat.as_str()) {
        return Err(ApiError::bad(St3Error::new(
            "foreign-agent-actor",
            format!(
                "this harness is `{seat}` and cannot act as `{}`",
                actor.unwrap_or("<none>")
            ),
        )));
    }
    let current = state
        .store
        .latest_claim(seat, Some("runtime.observed"))
        .map_err(ApiError::internal)?
        .filter(|runtime| runtime.body["fields"]["status"] == "running")
        .and_then(|runtime| {
            runtime.body["fields"]["incarnation_id"]
                .as_str()
                .map(str::to_owned)
        });
    if incarnation.is_none() {
        *incarnation = current.clone();
    }
    Ok(current)
}

pub async fn ask_person(
    State(state): State<AppState>,
    bound: Option<Extension<BoundAgent>>,
    Json(mut request): Json<PersonAskRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let expected = bind_work_incarnation(
        &state,
        bound.as_ref(),
        Some(&request.actor),
        &mut request.incarnation,
    )?;
    if bound.is_some()
        && request.step.is_some()
        && request.new_run.is_none()
        && request.incarnation != expected
    {
        return Err(ApiError::bad(St3Error::new(
            "stale-work-ask",
            format!(
                "only the current claimant and incarnation of live work can ask a person; expected incarnation `{}`, given `{}`",
                expected.as_deref().unwrap_or("<none>"),
                request.incarnation.as_deref().unwrap_or("<none>")
            ),
        )));
    }
    if let Some(structured) = request.request.as_mut() {
        complete_request_images(&state, &request.actor, structured)?;
    }
    let result = state.store.ask_person(&request).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

/// A request names its images by the asker's uploads, as a send does; the ask claim keeps each
/// completed with its size and the member holding the bytes (`docs/st3/attachments.md`).
fn complete_request_images(
    state: &AppState,
    asker: &str,
    request: &mut Value,
) -> Result<(), ApiError> {
    let Some(inputs) = request.get("attachments").filter(|inputs| !inputs.is_null()) else {
        return Ok(());
    };
    let inputs: Vec<crate::model::AttachmentInput> = serde_json::from_value(inputs.clone()).map_err(|error| {
        ApiError::bad(St3Error::new(
            "invalid-person-request",
            format!(
                "each image names an upload, {{\"blob\":\"blob/<sha256>\",\"media_type\":\"image/png\",\"name\":\"sketch.png\"}}: {error}"
            ),
        ))
    })?;
    let completed = client_blobs::resolve_attachments(state, asker, &inputs)?;
    request["attachments"] = serde_json::to_value(completed).map_err(ApiError::internal)?;
    Ok(())
}

pub async fn done_person_step(
    State(state): State<AppState>,
    Json(request): Json<PersonStepResponse>,
) -> Result<Json<StepRunView>, ApiError> {
    let result = state
        .store
        .finish_person_step(&request, false)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

pub async fn cancel_person_ask(
    State(state): State<AppState>,
    Json(request): Json<PersonStepResponse>,
) -> Result<Json<StepRunView>, ApiError> {
    let result = state
        .store
        .finish_person_step(&request, true)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

pub async fn list_work(
    State(state): State<AppState>,
    Query(query): Query<WorkQuery>,
) -> Result<Json<Vec<StepRunView>>, ApiError> {
    let store = state.store.clone();
    if matches!(query.view, WorkView::Renewal) {
        let actor = query.actor.ok_or_else(|| ApiError::bad(St3Error::new(
            "missing-work-actor", "the renewal work view requires an actor",
        )))?;
        return blocking_store(move || {
            crate::performance::task("work/renewal", || store.work_for_renewal(&actor))
        }).await.map(Json);
    }
    let (mut work, desired) = blocking_store(move || {
        let work = crate::performance::task(query.caller.label(), || {
            store.work(query.actor.as_deref(), query.include_terminal)
        })?;
        // Only the agents these steps name, not every subject the fleet has ever declared.
        let actors = work
            .iter()
            .flat_map(|step| {
                step.claimant
                    .iter()
                    .chain(step.assigned_to.iter())
                    .chain(step.available_to.iter())
                    .cloned()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let desired = store.desired_subjects_named(&actors)?;
        Ok((work, desired))
    })
    .await?;
    let agents = desired
        .into_iter()
        .filter(|subject| subject.kind == "agent")
        .map(|subject| (subject.subject, crate::graph::agent_under(&subject.desired)))
        .collect::<BTreeMap<_, _>>();
    for step in &mut work {
        if let Some(actor) = step
            .claimant
            .as_ref()
            .or(step.assigned_to.as_ref())
            .or_else(|| (step.available_to.len() == 1).then(|| &step.available_to[0]))
        {
            step.under = agents.get(actor).cloned().unwrap_or_default();
        }
    }
    Ok(Json(work))
}

pub async fn get_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<StepRunView>, ApiError> {
    let subject = if subject.starts_with("step-run/") {
        subject
    } else {
        format!("step-run/{subject}")
    };
    state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))
}

pub async fn wake_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<WorkWakeRequest>,
) -> Result<Json<MessageView>, ApiError> {
    if request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "missing-wake-reason",
            "a manual work wake needs a reason",
        )));
    }
    let step = state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let agent = step.assigned_to.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "work-has-no-assignee",
            format!("step run `{}` has no exact assignee to wake", step.subject),
        ))
    })?;
    let allowed = request
        .actor
        .strip_prefix("person/")
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        || normalized_agent_actor(&request.actor).is_some();
    if !allowed {
        return Err(ApiError::bad(St3Error::new(
            "invalid-wake-actor",
            format!("`{}` is neither a person nor an agent", request.actor),
        )));
    }
    if let Some(existing) = state
        .store
        .operation_claim(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        let message = state
            .store
            .message(&existing.subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the wake operation message is unavailable"))?;
        return Ok(Json(message));
    }
    if step.status != "ready" {
        return Err(ApiError::bad(St3Error::new(
            "work-not-ready",
            format!(
                "step run `{}` is `{}`, not ready",
                step.subject, step.status
            ),
        )));
    }
    let harness = state
        .store
        .current_harness(agent)
        .map_err(ApiError::internal)?
        .filter(crate::model::CurrentHarnessView::is_ready)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "assignee-harness-not-ready",
                format!("assignee `{agent}` has no ready current harness"),
            ))
        })?;
    if !state
        .store
        .fresh_context_ready(&step, agent, &harness.incarnation_id)
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::bad(St3Error::new(
            "fresh-context-pending",
            format!(
                "`{agent}` must start a fresh harness session before waking `{}`",
                step.subject
            ),
        )));
    }
    let attempt = step
        .wake
        .as_ref()
        .map(|wake| wake.attempts)
        .unwrap_or_default()
        .saturating_add(1);
    let message = crate::reconcile::append_work_wake_message(
        &state.store,
        &step,
        agent,
        &harness.incarnation_id,
        attempt,
        "manual",
        &request.actor,
        &request.reason,
        request.idempotency_key,
    )
    .map_err(ApiError::internal)?;
    signal_changed(&state);
    Ok(Json(message))
}

/// Retry one failed step as a person or an agent.
pub async fn retry_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<WorkRetryRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    if request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "missing-retry-reason",
            "a work retry needs a reason",
        )));
    }
    if let Some(cached) = state
        .store
        .cached_idempotency_response::<MissionRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(cached));
    }
    let actor = match request.actor.as_str() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "retry-authority-denied",
                "a work retry needs a person or agent actor",
            ))
        })?,
    };
    state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let store = state.store.clone();
    let retried = blocking_action(move || {
        store.retry_failed_step(&subject, &actor, &request.reason, &request.idempotency_key)
    })
    .await?;
    signal_changed(&state);
    Ok(Json(retried))
}

pub async fn publish_work_mission(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    bound: Option<Extension<BoundAgent>>,
    Json(mut request): Json<MissionProductionRequest>,
) -> Result<Json<MissionOutputView>, ApiError> {
    bind_work_incarnation(
        &state,
        bound.as_ref(),
        Some(&request.actor),
        &mut request.incarnation,
    )?;
    let step = state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let actor = if request.actor.contains('/') {
        request.actor.clone()
    } else {
        format!("agent/{}", request.actor)
    };
    if !state
        .store
        .mission_output_authorized(&step.subject, &actor, request.incarnation.as_deref())
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::bad(St3Error::new(
            "work-not-claimed",
            format!(
                "`{actor}` does not hold the mission-producing step `{}` or its nested work",
                step.subject
            ),
        )));
    }
    let run = state
        .store
        .mission_run(&step.run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{}` does not exist", step.run)))?;
    let root = state
        .store
        .mission_spec(
            run.mission.strip_prefix("mission/").unwrap_or(&run.mission),
            Some(&run.revision),
        )
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-mission-revision",
                "the running mission revision is unavailable",
            ))
        })?;
    let definition = crate::mission::find_step(&root, &step.step).ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-step-definition",
            format!("step `{}` is absent from its mission revision", step.step),
        ))
    })?;
    let expected_mission = definition.produces_mission.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "step-does-not-produce-mission",
            format!("step `{}` does not declare produces-mission", step.step),
        ))
    })?;

    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    if initial.missions.len() != 1 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-output-intent",
            "a mission output must contain exactly one mission",
        )));
    }
    if !initial.subjects.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-output-intent",
            "a mission output can contain only its mission",
        )));
    }
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let mission = intent
        .missions
        .values()
        .next()
        .expect("one mission was checked");
    if mission.id != expected_mission || mission.state != crate::model::MissionState::Ready {
        return Err(ApiError::bad(St3Error::new(
            "wrong-mission-output",
            format!(
                "step `{}` must publish ready mission `{expected_mission}`",
                step.step
            ),
        )));
    }
    let mut publication = intent.clone();
    publication.subjects.clear();
    let mut planned = state
        .store
        .mission(
            &publication,
            crate::model::IntentInput {
                kdl: resolved_kdl,
                source_name: request.intent.source_name,
            },
        )
        .map_err(ApiError::bad)?;
    publication_refusals(&state, &publication)
        .await?
        .block(&mut planned);
    if !planned.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "mission-output-blocked",
            planned.blockers.join("; "),
        )));
    }
    state
        .store
        .apply(
            &publication,
            &planned.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let output = state
        .store
        .record_mission_output(
            &step.subject,
            &actor,
            request.incarnation.as_deref(),
            expected_mission,
            mission,
            &format!("{}:bind", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(output))
}

/// Move one run in a seat's queue as a person or an agent.
pub async fn move_agent_queue(
    State(state): State<AppState>,
    Json(mut request): Json<crate::model::SeatQueueMoveRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request.agent = if request.agent.starts_with("agent/") {
        request.agent
    } else {
        format!("agent/{}", request.agent)
    };
    request.actor = match request.actor.as_str() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-queue-move-actor",
                "a queue move needs a person or agent actor",
            ))
        })?,
    };
    let store = state.store.clone();
    let claim = blocking_action(move || store.move_seat_queue_run(&request)).await?;
    signal_changed(&state);
    Ok(Json(claim))
}

pub async fn start_work(
    State(state): State<AppState>,
    Json(request): Json<crate::model::WorkStartRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let store = state.store.clone();
    let result = blocking_action(move || store.start_work(&request)).await?;
    signal_changed(&state);
    Ok(Json(result))
}

pub async fn handoff_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    bound: Option<Extension<BoundAgent>>,
    Json(mut input): Json<crate::model::WorkHandoffRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    bind_work_incarnation(
        &state,
        bound.as_ref(),
        Some(&input.actor),
        &mut input.incarnation,
    )?;
    let request = WorkRequest {
        wait_ms: None,
        actor: Some(input.actor.clone()),
        incarnation: input.incarnation.clone(),
        summary: Some(input.note.clone()),
        reason: None,
        evidence: input.evidence.clone(),
        idempotency_key: input.idempotency_key.clone(),
    };
    work_action_response(state, "release".into(), subject, request, None, Some(input)).await
}

pub async fn acknowledge_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<crate::model::WorkAcknowledgeRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let store = state.store.clone();
    let result = blocking_action(move || store.acknowledge_work(&subject, &request)).await?;
    signal_changed(&state);
    Ok(Json(result))
}

pub async fn post_work_action(
    State(state): State<AppState>,
    AxumPath((action, subject)): AxumPath<(String, String)>,
    bound: Option<Extension<BoundAgent>>,
    Json(mut request): Json<WorkRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    bind_work_incarnation(
        &state,
        bound.as_ref(),
        request.actor.as_deref(),
        &mut request.incarnation,
    )?;
    work_action_response(state, action, subject, request, None, None).await
}

/// Add time to the execution budget of the step attempt this seat holds.
pub async fn extend_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    bound: Option<Extension<BoundAgent>>,
    Json(mut request): Json<crate::model::WorkExtendRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    bind_work_incarnation(
        &state,
        bound.as_ref(),
        request.actor.as_deref(),
        &mut request.incarnation,
    )?;
    let extend_ms = request.by_ms;
    let request = WorkRequest {
        wait_ms: None,
        actor: request.actor,
        incarnation: request.incarnation,
        summary: None,
        reason: request.reason,
        evidence: Vec::new(),
        idempotency_key: request.idempotency_key,
    };
    work_action_response(state, "extend".into(), subject, request, Some(extend_ms), None).await
}

pub async fn post_gate_result(
    State(state): State<AppState>,
    Json(request): Json<GateResultRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    if !matches!(request.verdict.as_str(), "pass" | "fail") {
        return Err(ApiError::bad(St3Error::new(
            "invalid-gate-result",
            "a gate-result verdict must be pass or fail",
        )));
    }
    let capability = state
        .store
        .capability(&request.operation_capability, "gate-result")
        .map_err(ApiError::bad)?;
    if capability.used {
        let prior = state
            .store
            .latest_claim(&capability.subject, Some("gate.result"))
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "used-capability",
                    "the gate-result capability was already consumed",
                ))
            })?;
        let same = prior
            .body
            .pointer("/fields/verdict")
            .and_then(Value::as_str)
            == Some(request.verdict.as_str())
            && prior.body.pointer("/fields/reason").and_then(Value::as_str)
                == Some(request.reason.as_str());
        if same {
            return Ok(Json(prior));
        }
        return Err(ApiError::bad(St3Error::new(
            "used-capability",
            "the gate-result capability was already used for another verdict",
        )));
    }
    let input = ClaimInput {
        subject: capability.subject.clone(),
        kind: "gate.result".into(),
        actor: None,
        fields: BTreeMap::from([
            ("verdict".into(), Value::String(request.verdict.clone())),
            ("reason".into(), Value::String(request.reason.clone())),
        ]),
        evidence: request.evidence.clone(),
        expected_subject: None,
        idempotency_key: Some(request.idempotency_key.clone()),
    };
    state
        .store
        .validate_claim_input(&input)
        .map_err(ApiError::bad)?;
    for evidence in &input.evidence {
        if !state
            .store
            .evidence_exists(evidence)
            .map_err(ApiError::internal)?
        {
            return Err(ApiError::bad(St3Error::new(
                "missing-evidence",
                format!("evidence claim `{evidence}` is not stored"),
            )));
        }
    }
    let capability = state
        .store
        .consume_capability(&request.operation_capability, "gate-result")
        .map_err(ApiError::bad)?;
    if capability.used {
        let prior = state
            .store
            .latest_claim(&capability.subject, Some("gate.result"))
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "used-capability",
                    "the gate-result capability was already consumed",
                ))
            })?;
        let same = prior
            .body
            .pointer("/fields/verdict")
            .and_then(Value::as_str)
            == Some(request.verdict.as_str())
            && prior.body.pointer("/fields/reason").and_then(Value::as_str)
                == Some(request.reason.as_str());
        if same {
            return Ok(Json(prior));
        }
        return Err(ApiError::bad(St3Error::new(
            "used-capability",
            "the gate-result capability was already used for another verdict",
        )));
    }
    let response = state.store.append_claim(&input).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}
