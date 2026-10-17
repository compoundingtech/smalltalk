use super::*;

pub async fn start_planning_session(
    State(state): State<AppState>,
    Json(request): Json<PlanningSessionStartRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let request_text = std::str::from_utf8(&request.request).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-request-not-text",
            "a launch request must contain valid UTF-8",
        ))
    })?;
    if request_text.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-request",
            "a launch request cannot be empty",
        )));
    }
    let target_run = request
        .run
        .as_deref()
        .map(|run| state.store.mission_run(run))
        .transpose()
        .map_err(ApiError::internal)?
        .flatten();
    if request.run.is_some() && target_run.is_none() {
        return Err(ApiError::not_found("the target mission run does not exist"));
    }
    if let Some(run) = &target_run
        && (!matches!(run.status.as_str(), "running" | "blocked") || run.phase != "normal")
    {
        return Err(ApiError::bad(St3Error::new(
            "mission-run-not-revisable",
            format!(
                "mission run `{}` is {} in its {} phase",
                run.subject, run.status, run.phase
            ),
        )));
    }
    let mission_id = target_run
        .as_ref()
        .map(|run| {
            run.mission
                .strip_prefix("mission/")
                .unwrap_or(&run.mission)
                .to_owned()
        })
        .unwrap_or_else(|| request.mission.clone());
    crate::mission::validate_mission_id(&mission_id).map_err(ApiError::bad)?;
    let id = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()))[..24].to_owned();
    let request_name = format!("doc/planning/{id}/request");
    let request_document = state
        .store
        .put_document(
            &request_name,
            &request.request,
            &None,
            &format!("{}:request", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let requester =
        normalize_planning_reviewer(request.requester.as_deref().unwrap_or("person/requester"));
    let provider = request
        .provider
        .as_deref()
        .unwrap_or(&state.planner_default.provider)
        .to_owned();
    if !matches!(
        provider.as_str(),
        "codex" | "claude" | "pi" | "omp" | "opencode"
    ) {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-planner",
            "the planner must use an eligible typed harness driver",
        )));
    }
    let inherit_default = provider == state.planner_default.provider;
    let planner_config = PlannerSpec {
        provider,
        model: request.model.clone().or_else(|| {
            inherit_default
                .then(|| state.planner_default.model.clone())
                .flatten()
        }),
        effort: request.effort.clone().or_else(|| {
            inherit_default
                .then(|| state.planner_default.effort.clone())
                .flatten()
        }),
    };
    if planner_config.provider == "opencode" && planner_config.effort.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-planner-effort",
            "the OpenCode harness does not accept an effort override",
        )));
    }
    let planner_alias = format!("agent/planner.{}", &id[..10]);
    let request_reference = format!("{}@{}", request_document.name, request_document.hash);
    let context_reference = if let Some(run) = &target_run {
        let mission = state
            .store
            .mission_spec(&mission_id, Some(&run.revision))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the target mission revision is unavailable"))?;
        let context = serde_json::to_vec_pretty(&json!({"mission_run": run, "mission": mission}))
            .map_err(ApiError::internal)?;
        let document = state
            .store
            .put_document(
                &format!("doc/planning/{id}/run-context"),
                &context,
                &None,
                &format!("{}:run-context", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        Some(format!("{}@{}", document.name, document.hash))
    } else {
        None
    };
    let expected_subject = state
        .store
        .selected_desired_token(&planner_alias)
        .map_err(ApiError::internal)?
        .into_iter()
        .collect();
    // The planner seat starts idle, so what it is asked to do arrives as its launch request.
    let references = context_reference
        .as_ref()
        .map(|context| format!("{request_reference}\n{context}"))
        .unwrap_or_else(|| request_reference.clone());
    let request_text = format!(
        "You are the durable {} planner for launch {id}. Use `st documents get` for each immutable document reference below. Write one Markdown mission and one complete version 2 KDL mission. The KDL mission ID must be `{mission_id}` and its state must be ready. You can submit named variants with `st launch submit {id} --variant NAME --markdown FILE --kdl KDL_FILE`. Use temporary files outside the workspace, and remove them after submission. Do not change the workspace. Do not publish or run the mission. Stay available for revision messages until approval or cancellation.\n\n{references}",
        planner_config.provider
    );
    let planner = quick_agent(
        &state,
        QuickAgentRequest {
            subject: planner_alias,
            worktree: request.workspace.clone(),
            model: planner_config.model.clone(),
            effort: planner_config.effort.clone(),
            arguments: Vec::new(),
            expected_subject,
            idempotency_key: format!("{}:planner", request.idempotency_key),
        },
        &planner_config.provider,
    )
    .await?;
    let session = state
        .store
        .create_planning_session(
            &id,
            &mission_id,
            &request_reference,
            &request.workspace,
            &requester,
            &planner.subject,
            &planner_config,
            target_run.as_ref().map(|run| run.subject.as_str()),
            target_run.as_ref().map(|run| run.generation.as_str()),
        )
        .map_err(ApiError::bad)?;
    send_planning_message(
        &state,
        &format!("planning-request:{id}"),
        &requester,
        &planner.subject,
        &request_text,
        "Launch request",
    )?;
    let mut started_fields = BTreeMap::from([
        (
            "mission".into(),
            Value::String(format!("mission/{}", session.mission)),
        ),
        ("request".into(), Value::String(request_reference)),
        ("planner".into(), Value::String(session.planner.clone())),
        (
            "planner_config".into(),
            serde_json::to_value(&planner_config).map_err(ApiError::internal)?,
        ),
        ("workspace".into(), Value::String(session.workspace.clone())),
        ("requester".into(), Value::String(session.requester.clone())),
    ]);
    if let Some(run) = &session.target_mission_run {
        started_fields.insert("target_run".into(), Value::String(run.clone()));
    }
    if let Some(generation) = &session.source_generation {
        started_fields.insert(
            "target_generation".into(),
            Value::String(generation.clone()),
        );
    }
    record_planning_event(
        &state,
        &session,
        "planning-session.started",
        Some(&requester),
        started_fields,
        &format!("{}:started", request.idempotency_key),
    )?;
    signal_changed(&state);
    Ok(Json(session))
}

pub async fn get_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || store.planning_session(&id_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("launch `{id}` does not exist")))
}

pub async fn submit_planning_candidate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningCandidateSubmitRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    submit_planning_variant(state, id, "default".into(), request).await
}

pub async fn submit_named_planning_candidate(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
    Json(request): Json<PlanningCandidateSubmitRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    submit_planning_variant(state, id, variant, request).await
}

async fn submit_planning_variant(
    state: AppState,
    id: String,
    variant: String,
    request: PlanningCandidateSubmitRequest,
) -> Result<Json<PlanningSessionView>, ApiError> {
    validate_planning_variant(&variant)?;
    let session = required_planning_session(&state, &id)?;
    let markdown = std::str::from_utf8(&request.markdown).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-markdown-not-text",
            "planning Markdown must contain valid UTF-8",
        ))
    })?;
    if markdown.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-markdown",
            "planning Markdown cannot be empty",
        )));
    }
    let kdl = std::str::from_utf8(&request.kdl).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-kdl-not-text",
            "planning KDL must contain valid UTF-8",
        ))
    })?;
    let (intent, _) = mission_source(&state, kdl, None)?;
    let mission = launch_candidate_mission(&intent, &session.mission).map_err(ApiError::bad)?;
    if mission.state != crate::model::MissionState::Ready {
        return Err(ApiError::bad(St3Error::new(
            "launch-mission-not-ready",
            "a planning candidate must contain a ready mission",
        )));
    }
    let mut content_hasher = Sha256::new();
    content_hasher.update(b"st3.planning-candidate.v1\0");
    content_hasher.update((request.markdown.len() as u64).to_be_bytes());
    content_hasher.update(&request.markdown);
    content_hasher.update((request.kdl.len() as u64).to_be_bytes());
    content_hasher.update(&request.kdl);
    let content_hash = hex::encode(content_hasher.finalize());
    let markdown_name = format!(
        "doc/planning/{}/candidate/{content_hash}/markdown",
        session.id
    );
    let kdl_name = format!("doc/planning/{}/candidate/{content_hash}/kdl", session.id);
    let markdown_document = state
        .store
        .put_document(
            &markdown_name,
            &request.markdown,
            &None,
            &format!("planning-candidate:{}:{content_hash}:markdown", session.id),
        )
        .map_err(ApiError::bad)?;
    let kdl_document = state
        .store
        .put_document(
            &kdl_name,
            &request.kdl,
            &None,
            &format!("planning-candidate:{}:{content_hash}:kdl", session.id),
        )
        .map_err(ApiError::bad)?;
    let response = state
        .store
        .add_planning_candidate(
            &session.id,
            &request.actor,
            &variant,
            &format!("{}@{}", markdown_document.name, markdown_document.hash),
            &format!("{}@{}", kdl_document.name, kdl_document.hash),
            &mission.revision,
        )
        .map_err(ApiError::bad)?;
    let candidate = response
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .map(|variant| &variant.candidate)
        .expect("the submitted planning candidate is visible");
    record_planning_event(
        &state,
        &response,
        "planning-session.candidate-submitted",
        Some(&request.actor),
        BTreeMap::from([
            ("variant".into(), Value::String(variant.clone())),
            ("candidate_revision".into(), Value::from(candidate.revision)),
            ("markdown".into(), Value::String(candidate.markdown.clone())),
            ("kdl".into(), Value::String(candidate.kdl.clone())),
            (
                "mission_revision".into(),
                Value::String(candidate.mission_revision.clone()),
            ),
        ]),
        &format!(
            "planning-candidate:{}:{variant}:{}",
            response.id, candidate.revision
        ),
    )?;
    signal_changed(&state);
    preview_planning_variant(state, id, variant).await
}

pub async fn preview_planning_candidate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    preview_planning_variant(state, id, "default".into()).await
}

pub async fn preview_named_planning_candidate(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    preview_planning_variant(state, id, variant).await
}

async fn preview_planning_variant(
    state: AppState,
    id: String,
    variant: String,
) -> Result<Json<PlanningSessionView>, ApiError> {
    validate_planning_variant(&variant)?;
    let session = required_planning_session(&state, &id)?;
    if session.status != "review" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-reviewable",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    let candidate = session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .map(|variant| &variant.candidate)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-launch-candidate",
                "the launch has no candidate",
            ))
        })?;
    let kdl = planning_document_text(&state, &candidate.kdl)?;
    let (intent, mission_response) = mission_source(&state, &kdl, None)?;
    let mission = &intent.missions[&session.mission];
    let mut graph = render_planning_graph(mission);
    if let Some(provenance) = mission_response.mission_provenance.get(&session.mission) {
        graph.push('\n');
        graph.push_str(&crate::provenance::render(provenance));
    }
    let diff = render_planning_diff(&mission_response);
    let mut normalized = serde_json::to_value(mission).map_err(ApiError::internal)?;
    if let Some(provenance) = mission_response.mission_provenance.get(&session.mission) {
        normalized["provenance"] = serde_json::to_value(provenance).map_err(ApiError::internal)?;
    }
    client_safe_json(&mut normalized);
    let diagnostics = launch_diagnostics(&mission_response.blockers, &mission_response.warnings);
    let hash = launch_preview_token_values(
        &session.id,
        &variant,
        candidate.revision,
        session.source_generation.as_deref(),
        &normalized,
        &diagnostics,
    )
    .map_err(ApiError::internal)?;
    if session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .and_then(|variant| variant.preview.as_ref())
        .is_some_and(|preview| {
            preview.candidate_revision == candidate.revision && preview.hash == hash
        })
    {
        return Ok(Json(session));
    }
    let response = state
        .store
        .save_planning_preview(
            &session.id,
            &variant,
            candidate.revision,
            &hash,
            &graph,
            &diff,
            &mission_response,
        )
        .map_err(ApiError::bad)?;
    let preview = response
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .and_then(|variant| variant.preview.as_ref())
        .expect("the saved launch preview is visible");
    record_planning_event(
        &state,
        &response,
        "planning-session.previewed",
        Some(&response.requester),
        BTreeMap::from([
            ("variant".into(), Value::String(variant.clone())),
            (
                "candidate_revision".into(),
                Value::from(preview.candidate_revision),
            ),
            ("preview_hash".into(), Value::String(preview.hash.clone())),
            ("store_index".into(), Value::from(preview.store_index)),
            ("graph".into(), Value::String(preview.graph.clone())),
            ("diff".into(), Value::String(preview.diff.clone())),
            (
                "mission".into(),
                serde_json::to_value(&preview.mission).map_err(ApiError::internal)?,
            ),
        ]),
        &format!("planning-preview:{}:{}", response.id, preview.hash),
    )?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn compare_planning_variants(
    State(state): State<AppState>,
    AxumPath((id, left, right)): AxumPath<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    let variant = |name: &str| {
        session
            .variants
            .iter()
            .find(|variant| variant.name == name)
            .ok_or_else(|| ApiError::not_found(format!("launch variant `{name}` does not exist")))
    };
    let left = variant(&left)?;
    let right = variant(&right)?;
    Ok(Json(json!({
        "session": session.subject,
        "source_generation": session.source_generation,
        "left": left,
        "right": right,
    })))
}

pub async fn propose_planning_variant(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
    Json(request): Json<PlanningProposalRequest>,
) -> Result<Json<RevisionSubmissionView>, ApiError> {
    if let Some(cached) = cached_revision_submission(
        &state,
        &format!("{}:cutover", request.idempotency_key),
        &format!("{}:proposal", request.idempotency_key),
    )? {
        return Ok(Json(cached));
    }
    let session = required_planning_session(&state, &id)?;
    let run_subject = session.target_mission_run.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "launch-has-no-run",
            "this launch creates a new mission and cannot propose a run revision",
        ))
    })?;
    let current = state
        .store
        .mission_run(run_subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("the target mission run does not exist"))?;
    if session.source_generation.as_deref() != Some(current.generation.as_str()) {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-generation",
            "the launch variants target a superseded generation",
        )));
    }
    let variant = session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .ok_or_else(|| ApiError::not_found(format!("launch variant `{variant}` does not exist")))?;
    let preview = variant.preview.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-preview",
            "preview the named variant before proposal",
        ))
    })?;
    if !preview.mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "launch-preview-blocked",
            preview.mission.blockers.join("; "),
        )));
    }
    let intent =
        parse_intent(&preview.mission.resolved_intent.kdl, &state.node).map_err(ApiError::bad)?;
    let mission = &intent.missions[&session.mission];
    let old = state
        .store
        .mission_spec(&session.mission, Some(&current.revision))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the current mission revision is unavailable"))?;
    let (_, reviewers) = crate::store::analyze_mission_revision(&old, mission, &current.requester)
        .map_err(ApiError::bad)?;
    state
        .store
        .apply(
            &intent,
            &preview.mission.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let result =
        if reviewers.is_empty() && matches!(old.revision_cutover, RevisionCutover::RestartActive) {
            RevisionSubmissionView {
                provenance: state
                    .store
                    .mission_provenance(&session.mission, &mission.revision)
                    .map_err(ApiError::internal)?,
                status: "applied".into(),
                mission_run: state
                    .store
                    .adopt_mission_revision(
                        run_subject,
                        mission,
                        &request.actor,
                        &request.reason,
                        &format!("{}:cutover", request.idempotency_key),
                    )
                    .map_err(ApiError::bad)?,
                proposal: None,
            }
        } else {
            let proposal = state
                .store
                .create_revision_proposal(
                    run_subject,
                    mission,
                    &request.actor,
                    &request.reason,
                    &format!("{}:proposal", request.idempotency_key),
                )
                .map_err(ApiError::bad)?;
            RevisionSubmissionView {
                provenance: state
                    .store
                    .mission_provenance(&session.mission, &mission.revision)
                    .map_err(ApiError::internal)?,
                status: proposal.status.clone(),
                mission_run: state
                    .store
                    .mission_run(run_subject)
                    .map_err(ApiError::internal)?
                    .expect("the target mission run exists"),
                proposal: Some(proposal),
            }
        };
    signal_changed(&state);
    Ok(Json(result))
}

fn validate_planning_variant(variant: &str) -> Result<(), ApiError> {
    if variant.is_empty()
        || variant.len() > 80
        || !variant
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-variant",
            "a launch variant must use 1 through 80 letters, digits, dots, dashes, or underscores",
        )));
    }
    Ok(())
}

pub async fn revise_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningRevisionRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if request.feedback.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-feedback",
            "launch feedback cannot be empty",
        )));
    }
    std::str::from_utf8(&request.feedback).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-feedback-not-text",
            "launch feedback must contain valid UTF-8",
        ))
    })?;
    let feedback_hash = hex::encode(Sha256::digest(&request.feedback));
    let name = format!("doc/planning/{}/feedback/{feedback_hash}", session.id);
    let document = state
        .store
        .put_document(
            &name,
            &request.feedback,
            &None,
            &format!("{}:feedback", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let response = state
        .store
        .request_planning_revision(&session.id, &request.actor)
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.revision-requested",
        Some(&request.actor),
        BTreeMap::from([(
            "feedback".into(),
            Value::String(format!("{}@{}", document.name, document.hash)),
        )]),
        &format!("{}:revision-requested", request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("planning-revision:{}:{feedback_hash}", session.id),
        &request.actor,
        &session.planner,
        &format!("{}@{}", document.name, document.hash),
        "Launch revision requested",
    )?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn approve_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningApprovalRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status != "review" && session.status != "approved" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-reviewable",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    let candidate = session.candidate.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-candidate",
            "the launch has no candidate",
        ))
    })?;
    let preview = session.preview.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-preview",
            "preview the candidate before approval",
        ))
    })?;
    let variant = session
        .variants
        .iter()
        .find(|variant| variant.name == candidate.variant)
        .ok_or_else(|| ApiError::internal("the selected launch variant is unavailable"))?;
    let projected =
        client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?;
    let approval_token = projected
        .iter()
        .find(|resource| {
            resource["id"] == format!("launch-variant/{}/{}", session.id, variant.name)
        })
        .and_then(|resource| resource["preview_token"].as_str())
        .ok_or_else(|| ApiError::internal("the selected launch preview has no approval token"))?;
    if (preview.hash != request.preview_hash && approval_token != request.preview_hash)
        || preview.candidate_revision != candidate.revision
    {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-preview",
            "the approval does not name the current preview",
        )));
    }
    if session.status == "approved" {
        if session.published_revision.as_deref() == Some(candidate.mission_revision.as_str()) {
            let approval_key = format!("planning-approval:{}:{}", session.id, preview.hash);
            stop_planning_agent(&state, &session.planner, &approval_key)?;
            signal_changed(&state);
            return Ok(Json(session));
        }
        return Err(ApiError::internal(format!(
            "launch `{}` has inconsistent approved revision state",
            session.id
        )));
    }
    if !preview.mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "launch-preview-blocked",
            preview.mission.blockers.join("; "),
        )));
    }
    let target = if let Some(run_subject) = session.target_mission_run.as_deref() {
        let current = state
            .store
            .mission_run(run_subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("the target mission run does not exist"))?;
        if session.source_generation.as_deref() != Some(current.generation.as_str()) {
            return Err(ApiError::bad(St3Error::new(
                "stale-launch-generation",
                "the launch approval targets a superseded generation",
            )));
        }
        Some((run_subject.to_owned(), current))
    } else {
        None
    };
    let intent =
        parse_intent(&preview.mission.resolved_intent.kdl, &state.node).map_err(ApiError::bad)?;
    let approval_key = format!("planning-approval:{}:{}", session.id, preview.hash);
    state
        .store
        .apply(
            &intent,
            &preview.mission.subject_tokens,
            &format!("{approval_key}:publish"),
        )
        .map_err(ApiError::bad)?;
    if let Some((run_subject, current)) = target {
        let mission = &intent.missions[&session.mission];
        let old = state
            .store
            .mission_spec(&session.mission, Some(&current.revision))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the current mission revision is unavailable"))?;
        let (_, reviewers) =
            crate::store::analyze_mission_revision(&old, mission, &current.requester)
                .map_err(ApiError::bad)?;
        if reviewers.is_empty() && matches!(old.revision_cutover, RevisionCutover::RestartActive) {
            state
                .store
                .adopt_mission_revision(
                    &run_subject,
                    mission,
                    &request.actor,
                    "the requester approved the launch revision",
                    &format!("{approval_key}:cutover"),
                )
                .map_err(ApiError::bad)?;
        } else {
            let proposal = state
                .store
                .create_revision_proposal(
                    &run_subject,
                    mission,
                    &request.actor,
                    "the requester approved the launch revision",
                    &format!("{approval_key}:proposal"),
                )
                .map_err(ApiError::bad)?;
            if proposal.reviewers.contains(&request.actor) {
                let revision_preview = proposal
                    .preview_hash
                    .as_deref()
                    .ok_or_else(|| ApiError::internal("the revision proposal has no preview"))?;
                state
                    .store
                    .approve_revision_proposal(
                        &proposal.subject,
                        &request.actor,
                        revision_preview,
                        &format!("{approval_key}:revision-approval"),
                    )
                    .map_err(ApiError::bad)?;
            }
        }
    }
    let response = state
        .store
        .finish_planning_session(
            &session.id,
            &request.actor,
            "approved",
            Some(&candidate.mission_revision),
        )
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.approved",
        Some(&request.actor),
        BTreeMap::from([
            ("variant".into(), Value::String(candidate.variant.clone())),
            ("candidate_revision".into(), Value::from(candidate.revision)),
            ("preview_hash".into(), Value::String(preview.hash.clone())),
            (
                "preview_token".into(),
                Value::String(approval_token.to_owned()),
            ),
            (
                "mission_revision".into(),
                Value::String(candidate.mission_revision.clone()),
            ),
            ("markdown".into(), Value::String(candidate.markdown.clone())),
            ("kdl".into(), Value::String(candidate.kdl.clone())),
            (
                "requester".into(),
                Value::String(response.requester.clone()),
            ),
        ]),
        &format!("{approval_key}:event"),
    )?;
    stop_planning_agent(&state, &session.planner, &approval_key)?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn start_approved_launch(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchStartRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status != "approved" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-approved",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    if session.target_mission_run.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "launch-target-already-running",
            "a launch targeting an existing run is applied in place and cannot start another run",
        )));
    }
    let revision = session.published_revision.clone().ok_or_else(|| {
        ApiError::internal(format!(
            "approved launch `{}` has no published revision",
            session.id
        ))
    })?;
    let response = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: session.mission,
            revision: Some(revision),
            workspace: request.workspace,
            requester: Some(normalize_message_party(&request.actor)),
            mode: None,
            inputs: request.inputs,
            idempotency_key: format!("launch-start:{}:{}", session.id, request.idempotency_key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

pub async fn approve_and_start_launch(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchApproveAndStartRequest>,
) -> Result<Json<LaunchApproveAndStartView>, ApiError> {
    let Json(launch) = approve_planning_session(
        State(state.clone()),
        AxumPath(id.clone()),
        Json(PlanningApprovalRequest {
            actor: request.actor.clone(),
            preview_hash: request.preview_hash,
            idempotency_key: format!("{}:approve", request.idempotency_key),
        }),
    )
    .await?;
    // Approval is intentionally committed before start. If runtime creation fails, the durable
    // approved launch remains recoverable through this same idempotent request or `/start`.
    let Json(mission_run) = start_approved_launch(
        State(state),
        AxumPath(id),
        Json(LaunchStartRequest {
            actor: request.actor,
            workspace: request.workspace,
            inputs: request.inputs,
            idempotency_key: format!("{}:start", request.idempotency_key),
        }),
    )
    .await?;
    Ok(Json(LaunchApproveAndStartView {
        launch,
        mission_run,
    }))
}

pub async fn request_launch_decision(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchDecisionRequest>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    if normalize_message_party(&request.actor) != session.planner {
        return Err(ApiError::bad(St3Error::new(
            "launch-decision-not-authorized",
            "only the launch planner can request a decision",
        )));
    }
    let question = request.question.trim();
    if question.is_empty() || question.len() > 2_000 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-question",
            "a launch question must contain 1 through 2000 bytes",
        )));
    }
    let options = request
        .options
        .iter()
        .map(|option| LaunchDecisionOption {
            id: option.id.trim().to_owned(),
            label: option.label.trim().to_owned(),
            description: option
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        })
        .collect::<Vec<_>>();
    let unique = options
        .iter()
        .map(|option| option.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let boolean = matches!(request.decision_type, LaunchDecisionType::Boolean);
    if (boolean && !options.is_empty())
        || (!boolean && options.is_empty())
        || options.len() > 20
        || options
            .iter()
            .any(|option| option.id.is_empty() || option.label.is_empty())
        || unique.len() != options.len()
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-options",
            "boolean questions have no options; choice and rank questions need 1 through 20 options with distinct non-empty IDs and labels",
        )));
    }
    let digest = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()));
    let decision_id = format!("launch-decision/{}", &digest[..32]);
    record_planning_event(
        &state,
        &session,
        "planning-session.question-requested",
        Some(&request.actor),
        BTreeMap::from([
            ("decision_id".into(), Value::String(decision_id.clone())),
            ("revision".into(), Value::from(1)),
            ("question".into(), Value::String(question.to_owned())),
            (
                "decision_type".into(),
                serde_json::to_value(&request.decision_type).map_err(ApiError::internal)?,
            ),
            (
                "options".into(),
                serde_json::to_value(&options).map_err(ApiError::internal)?,
            ),
            ("requester".into(), Value::String(session.requester.clone())),
            ("planner".into(), Value::String(session.planner.clone())),
        ]),
        &format!("launch-question:{}:{}", session.id, request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("launch-question-message:{}:{decision_id}", session.id),
        &session.planner,
        &session.requester,
        &format!(
            "{decision_id}\n{question}\nDecision: {}",
            serde_json::to_string(&json!({"type": request.decision_type, "options": options}))
                .map_err(ApiError::internal)?
        ),
        "Launch decision requested",
    )?;
    signal_changed(&state);
    let value = client_launch_decision_resources(&state.store, &session)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| ApiError::internal("the recorded launch decision is unavailable"))?;
    Ok(Json(value))
}

pub async fn answer_launch_decision(
    State(state): State<AppState>,
    AxumPath((id, decision)): AxumPath<(String, String)>,
    Json(request): Json<LaunchDecisionAnswerRequest>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    let decision_id = client_detail_id("launch-decision", &decision);
    let decisions =
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?;
    let current = decisions
        .iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| {
            ApiError::not_found(format!("launch decision `{decision_id}` does not exist"))
        })?;
    if request.expected_revision != 1 {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-decision",
            "the launch decision revision changed",
        )));
    }
    let response = serde_json::to_value(&request.response).map_err(ApiError::internal)?;
    let explanation = request
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if current["state"] == "answered" {
        if current["response"] == response
            && current["explanation"]
                == serde_json::to_value(&explanation).map_err(ApiError::internal)?
        {
            return Ok(Json(current.clone()));
        }
        return Err(ApiError::bad(St3Error::new(
            "launch-decision-immutable",
            "a launch decision answer is immutable",
        )));
    }
    let decision_type: LaunchDecisionType =
        serde_json::from_value(current["decision_type"].clone())
            .map_err(|_| ApiError::internal("launch decision has an invalid type"))?;
    let options: Vec<LaunchDecisionOption> = serde_json::from_value(current["options"].clone())
        .map_err(|_| ApiError::internal("launch decision has invalid options"))?;
    let valid = valid_launch_decision_response(&decision_type, &options, &request.response);
    if !valid {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-answer",
            "the structured response must match the decision type and contain unique option IDs from the offered options; rank responses must include every option exactly once",
        )));
    }
    let mut answer_fields = BTreeMap::from([
        ("decision_id".into(), Value::String(decision_id.clone())),
        (
            "expected_revision".into(),
            Value::from(request.expected_revision),
        ),
        ("response".into(), response.clone()),
        ("requester".into(), Value::String(session.requester.clone())),
    ]);
    if let Some(explanation) = &explanation {
        answer_fields.insert("explanation".into(), Value::String(explanation.clone()));
    }
    record_planning_event(
        &state,
        &session,
        "planning-session.question-answered",
        Some(&request.actor),
        answer_fields,
        &format!("launch-answer:{}:{}", session.id, request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("launch-answer-message:{}:{decision_id}", session.id),
        &session.requester,
        &session.planner,
        &format!(
            "{decision_id}\nResponse: {}",
            serde_json::to_string(&response).map_err(ApiError::internal)?
        ),
        "Launch decision answered",
    )?;
    close_planning_message(
        &state,
        &format!("launch-question-message:{}:{decision_id}", session.id),
        &session.requester,
        &request.idempotency_key,
    )?;
    signal_changed(&state);
    let value = client_launch_decision_resources(&state.store, &session)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| ApiError::internal("the answered launch decision is unavailable"))?;
    Ok(Json(value))
}

pub async fn cancel_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningCancelRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status == "cancelled" {
        stop_planning_agent(&state, &session.planner, &request.idempotency_key)?;
        signal_changed(&state);
        return Ok(Json(session));
    }
    let response = state
        .store
        .finish_planning_session(&session.id, &request.actor, "cancelled", None)
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.cancelled",
        Some(&request.actor),
        BTreeMap::from([
            (
                "reason".into(),
                request
                    .reason
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ),
            (
                "requester".into(),
                Value::String(response.requester.clone()),
            ),
        ]),
        &format!("{}:cancelled", request.idempotency_key),
    )?;
    stop_planning_agent(&state, &session.planner, &request.idempotency_key)?;
    signal_changed(&state);
    Ok(Json(response))
}

fn required_planning_session(state: &AppState, id: &str) -> Result<PlanningSessionView, ApiError> {
    // Legacy CLI launch IDs can themselves begin with `launch/`. Resolve the exact stored
    // ID before treating that prefix as the client resource namespace.
    client_launch_session(state, id)
}

fn normalize_planning_reviewer(value: &str) -> String {
    if value.contains('/') {
        value.to_owned()
    } else {
        format!("person/{value}")
    }
}

fn authorize_planning_reviewer(session: &PlanningSessionView, actor: &str) -> Result<(), ApiError> {
    if normalize_planning_reviewer(actor) == session.requester {
        return Ok(());
    }
    Err(ApiError::bad(St3Error::new(
        "launch-review-not-authorized",
        format!("`{actor}` cannot review launch `{}`", session.id),
    )))
}

fn record_planning_event(
    state: &AppState,
    session: &PlanningSessionView,
    kind: &str,
    actor: Option<&str>,
    fields: BTreeMap<String, Value>,
    idempotency_key: &str,
) -> Result<(), ApiError> {
    state
        .store
        .append_claim(&ClaimInput {
            subject: session.subject.clone(),
            kind: kind.into(),
            actor: actor.map(normalize_message_party),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(idempotency_key.into()),
        })
        .map_err(ApiError::bad)?;
    state
        .store
        .rebuild_planning_projection()
        .map_err(ApiError::internal)?;
    Ok(())
}

fn planning_document_text(state: &AppState, reference: &str) -> Result<String, ApiError> {
    let (name, hash) = reference.rsplit_once('@').ok_or_else(|| {
        ApiError::internal(format!(
            "planning document reference `{reference}` is invalid"
        ))
    })?;
    let bytes = state
        .store
        .get_document(name, hash)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal(format!("planning document `{reference}` is missing")))?;
    String::from_utf8(bytes).map_err(ApiError::internal)
}

fn render_planning_graph(mission: &crate::model::MissionSpec) -> String {
    fn append(mission: &crate::model::MissionSpec, indent: &str, lines: &mut Vec<String>) {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            let dependencies = step
                .dependencies
                .iter()
                .filter_map(|dependency| match dependency {
                    crate::model::DependencySpec::Step { step, .. } => Some(step.as_str()),
                    crate::model::DependencySpec::Predicate { .. } => None,
                })
                .collect::<Vec<_>>();
            let suffix = if dependencies.is_empty() {
                "root".into()
            } else {
                format!("after {}", dependencies.join(", "))
            };
            let queue = step
                .queue
                .as_deref()
                .zip(step.queue_position)
                .map(|(queue, position)| format!(" · queue {queue} #{position}"))
                .unwrap_or_default();
            lines.push(format!("{indent}{} [{suffix}]{queue}", step.path));
            for product in &step.products {
                lines.push(format!("{indent}  produces {}", product.subject));
            }
            for gate in &step.gates {
                lines.push(format!("{indent}  gate {}", crate::gate_parse::gate_name(gate)));
            }
            if let Some(nested) = &step.nested_mission {
                append(nested, &format!("{indent}  "), lines);
            }
        }
    }
    let mut lines = vec![format!("mission/{}", mission.id)];
    for baseline in &mission.baselines {
        lines.push(format!("  baseline {}", baseline.name));
    }
    for product in &mission.products {
        lines.push(format!("  produces {}", product.subject));
    }
    for gate in &mission.gates {
        lines.push(format!("  gate {}", crate::gate_parse::gate_name(gate)));
    }
    append(mission, "  ", &mut lines);
    lines.join("\n")
}

fn render_planning_diff(mission: &MissionResponse) -> String {
    if mission.changes.is_empty() {
        return "No graph changes.".into();
    }
    mission
        .changes
        .iter()
        .map(|change| format!("{} {}", change.change, change.subject))
        .collect::<Vec<_>>()
        .join("\n")
}

fn close_planning_message(
    state: &AppState,
    message_key: &str,
    actor: &str,
    operation_key: &str,
) -> Result<(), ApiError> {
    let subject = format!(
        "message/{}",
        &hex::encode(Sha256::digest(message_key.as_bytes()))[..16]
    );
    for (kind, status) in [
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: kind.into(),
                actor: Some(normalize_message_party(actor)),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{operation_key}:{status}")),
            })
            .map_err(ApiError::bad)?;
    }
    Ok(())
}

fn stop_planning_agent(state: &AppState, planner: &str, key: &str) -> Result<(), ApiError> {
    let local_agent = planner
        .strip_prefix("agent/")
        .and_then(|value| value.split_once('/').map(|(_, local)| local))
        .unwrap_or(planner);
    let standing_mission = format!("mission/standing/{local_agent}");
    let cancellation = state
        .store
        .active_mission_runs()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|run| run.mission == standing_mission)
        .map(|run| run.subject)
        .map(|run| {
            format!(
                "mission-run {:?} {{ cancellation \"planning-session-ended\" {{ reason \"the launch ended\" }} }}\n",
                run
            )
        })
        .unwrap_or_default();
    let kdl = if cancellation.is_empty() {
        format!("version 2\nstop {planner:?}\n")
    } else {
        format!("version 2\n{cancellation}")
    };
    let intent = crate::graph::parse_internal_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    state
        .store
        .apply_internal(&intent, &format!("{key}:stop-planner"))
        .map_err(ApiError::bad)?;
    Ok(())
}

pub async fn mission(
    State(state): State<AppState>,
    Json(request): Json<MissionRequest>,
) -> Result<Json<MissionResponse>, ApiError> {
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, request.at_index)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let resolved = crate::model::IntentInput {
        kdl: resolved_kdl,
        source_name: request.intent.source_name,
    };
    let mut response = state
        .store
        .mission_at(&intent, resolved, request.at_index)
        .map_err(ApiError::bad)?;
    publication_refusals(&state, &intent)
        .await?
        .block(&mut response);
    response
        .warnings
        .extend(ignored_authority_warnings(&intent, &state.node).map_err(ApiError::bad)?);
    Ok(Json(response))
}

/// One preview warning for each agent the publication declares, directly or inside a mission,
/// that carries authority blocks, which free mode ignores.
fn ignored_authority_warnings(
    intent: &crate::model::NormalizedIntent,
    node: &str,
) -> Result<Vec<String>, St3Error> {
    let mut ignored = intent
        .subjects
        .values()
        .filter(|desired| desired.kind == "agent")
        .map(|desired| {
            (
                desired.subject.clone(),
                crate::graph::declared_authority_blocks(&desired.desired),
            )
        })
        .filter(|(_, blocks)| !blocks.is_empty())
        .collect::<BTreeMap<_, _>>();
    for id in crate::mission::top_level_mission_ids(&intent.missions) {
        if let Some(mission) = intent.missions.get(&id) {
            ignored.extend(crate::graph::mission_declared_authority_blocks(
                mission, node,
            )?);
        }
    }
    Ok(ignored
        .into_iter()
        .map(|(subject, blocks)| {
            format!(
                "free-mode: `{subject}` declares {}, which st ignores; within a fleet every agent may do what its person may do",
                blocks
                    .iter()
                    .map(|block| format!("`{block}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect())
}

pub async fn apply_with_bound(
    State(state): State<AppState>,
    bound: Option<Extension<BoundAgent>>,
    Json(request): Json<ApplyRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    apply_with_authority(state, request, bound.as_ref().map(|bound| bound.0.0.as_str())).await
}

pub async fn get_mission(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let id = id.strip_prefix("mission/").unwrap_or(&id).to_owned();
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || {
        let Some(mission) = store.mission_spec(&id_for_read, None)? else {
            return Ok(None);
        };
        let provenance = store.mission_provenance(&id_for_read, &mission.revision)?;
        let mut value = serde_json::to_value(mission)?;
        if let Some(provenance) = provenance {
            value["provenance"] = serde_json::to_value(provenance)?;
        }
        Ok(Some(value))
    })
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found(format!("mission `mission/{id}` does not exist")))
}

/// A person or an agent retires a mission.
pub async fn retire_mission(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<MissionRetireRequest>,
) -> Result<Json<crate::model::MissionSpec>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "mission-retire-authority-denied")?;
    let store = state.store.clone();
    let retired =
        blocking_action(move || store.retire_mission(&id, &actor, &request.idempotency_key))
            .await?;
    signal_changed(&state);
    Ok(Json(retired))
}
