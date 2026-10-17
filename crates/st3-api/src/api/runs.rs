use super::*;

/// Start running each exec gate of a mission file once, the way a run would: `st missions check`.
/// The answer lists every gate; poll `GET /v1/gate-checks/{id}` until it is finished.
pub async fn start_gate_check(
    State(state): State<AppState>,
    Json(request): Json<crate::model::GateCheckRequest>,
) -> Result<Json<crate::model::GateCheckView>, ApiError> {
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    // Check the commands a publication would store: document names pinned to their versions.
    let intent = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .ok()
        .and_then(|bindings| resolve_document_references(&request.intent.kdl, &bindings).ok())
        .and_then(|resolved| parse_intent(&resolved, &state.node).ok())
        .unwrap_or(initial);
    let workspace = std::path::Path::new(&request.workspace);
    if !workspace.is_absolute() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-check-workspace",
            "a gate check's workspace must be an absolute path",
        )));
    }
    let (node, state_dir, pty_root) = (state.node, state.state_dir, state.pty_root);
    let view = blocking_external(move || {
        crate::gate_check::start(
            crate::gate_check::CheckHost {
                node: &node,
                state_dir: &state_dir,
                pty_root: &pty_root,
            },
            &intent,
            &request.workspace,
            &request.inputs,
        )
    })
    .await?
    .map_err(ApiError::internal)?;
    Ok(Json(view))
}

pub async fn read_gate_check(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<crate::model::GateCheckView>, ApiError> {
    crate::api::read_deadline::spawn_blocking(move || crate::gate_check::poll(&id))
        .await
        .map_err(|error| ApiError::internal(anyhow::anyhow!(error)))?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found(
                "no such gate check: it started over an hour ago or the daemon restarted",
            )
        })
}

pub async fn start_eval(
    State(state): State<AppState>,
    Json(request): Json<EvalStartRequest>,
) -> Result<Json<EvalStartResponse>, ApiError> {
    let actual_hash = hex::encode(Sha256::digest(&request.bundle));
    if actual_hash != request.bundle_hash {
        return Err(ApiError::bad(St3Error::new(
            "eval-bundle-hash-mismatch",
            "the eval bundle bytes do not match the supplied hash",
        )));
    }
    state
        .store
        .put_blob(&request.bundle)
        .map_err(ApiError::internal)?;
    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let workspace = state
        .state_dir
        .join("evals")
        .join(&request.bundle_hash[..16])
        .join(nonce.to_string());
    hydrate_eval(&request.bundle, &workspace).map_err(ApiError::internal)?;
    let eval_file = workspace.join("eval.kdl");
    let kdl = fs::read_to_string(&eval_file)
        .map_err(|error| ApiError::internal(format!("read {}: {error}", eval_file.display())))?;
    let kdl = kdl.replace("${EVAL_ROOT}", &workspace.to_string_lossy());
    let mut intent = parse_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    let ready = intent
        .missions
        .values()
        .filter(|mission| mission.state == crate::model::MissionState::Ready)
        .collect::<Vec<_>>();
    let named_entry = format!("eval/{}", request.name);
    let entry = ready
        .iter()
        .copied()
        .find(|mission| mission.id == named_entry)
        .or_else(|| (ready.len() == 1).then_some(ready[0]))
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-eval-mission-count",
                format!(
                    "an eval with helper missions needs one ready entry mission named `{named_entry}`"
                ),
            ))
        })?;
    let timeout_ms = entry.timeout_ms.ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-eval-timeout",
            format!("eval entry mission `{}` needs a timeout", entry.id),
        ))
    })?;
    if timeout_ms > MAX_EVAL_TIMEOUT_MS {
        return Err(ApiError::bad(St3Error::new(
            "eval-timeout-too-large",
            format!(
                "eval entry mission `{}` timeout exceeds the 20 minute limit",
                entry.id
            ),
        )));
    }
    let entry_id = entry.id.clone();
    let entry_revision = entry.revision.clone();
    let idempotency_key = format!("eval:{}:{}:{nonce}", request.name, request.bundle_hash);
    let owner_run = state
        .store
        .mission_run_subject_for_idempotency_key(&idempotency_key);
    scope_eval_desired_subjects(
        &mut intent,
        &state.store.desired_subjects().map_err(ApiError::internal)?,
        &owner_run,
    )
    .map_err(ApiError::bad)?;
    stage_eval_documents(&state, &workspace, &intent).map_err(ApiError::bad)?;
    let mission = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.clone(),
                source_name: Some(eval_file.display().to_string()),
            },
        )
        .map_err(ApiError::bad)?;
    if !mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "eval-blocked",
            mission.blockers.join("; "),
        )));
    }
    let applied = state
        .store
        .apply(
            &intent,
            &mission.subject_tokens,
            &format!(
                "eval:{}:{}:{nonce}:apply",
                request.name, request.bundle_hash
            ),
        )
        .map_err(ApiError::bad)?;
    let run = match state.store.create_mission_run(&MissionRunRequest {
        mission: entry_id,
        revision: Some(entry_revision),
        workspace: workspace.to_string_lossy().into_owned(),
        requester: Some("person/eval-requester".into()),
        mode: Some("eval".into()),
        inputs: request.inputs,
        idempotency_key,
    }) {
        Ok(run) => run,
        Err(error) => {
            state
                .store
                .discard_desired_owned_by(&owner_run)
                .map_err(ApiError::internal)?;
            return Err(ApiError::bad(error));
        }
    };
    signal_changed(&state);
    Ok(Json(EvalStartResponse {
        event_cursor: applied
            .store_index
            .max(state.store.index().map_err(ApiError::internal)?),
        mission_run: run.subject,
    }))
}

pub async fn get_eval(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<EvalStatus>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    let run = blocking_store(move || store.mission_run(&run_for_read))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("eval mission run `{run}` does not exist")))?;
    let store = state.store.clone();
    let run_subject = run.subject.clone();
    let verdict_claim =
        blocking_store(move || store.latest_claim(&run_subject, Some("eval.verdict"))).await?;
    let verdict = verdict_claim
        .as_ref()
        .and_then(|claim| claim.body.pointer("/fields/verdict"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cleanup = if run.phase == "terminal" && verdict.is_some() {
        "complete"
    } else if run.phase == "final"
        || run.phase == "final-cancelled"
        || run.phase.starts_with("cleanup-")
    {
        "cleaning"
    } else {
        "pending"
    };
    let active_steps = run
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.status.as_str(),
                "ready" | "claimed" | "working" | "verifying" | "blocked"
            )
        })
        .map(|step| step.step.clone())
        .collect();
    Ok(Json(EvalStatus {
        mission_run: run.subject,
        lifecycle: run.status,
        phase: run.phase,
        active_steps,
        verdict,
        cleanup: cleanup.into(),
        store_index: state.store.index().map_err(ApiError::internal)?,
    }))
}

#[derive(Deserialize)]
pub struct MissionRunQuery {
    root: Option<String>,
    mission: Option<String>,
}

#[derive(Deserialize)]
pub struct MissionRunTreeQuery {
    root: String,
    after: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct MissionOverviewQuery {
    mission: String,
}

pub async fn mission_overview(
    State(state): State<AppState>,
    Query(query): Query<MissionOverviewQuery>,
) -> Result<Json<Value>, ApiError> {
    blocking_store(move || {
        state
            .store
            .read_snapshot(|_| state.store.mission_overview(&query.mission, 10))
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
pub struct OutcomeHistoryQuery {
    collection: String,
    #[serde(default)]
    since: u64,
    until: Option<u64>,
    status: Option<String>,
    actor: Option<String>,
    before: Option<u64>,
    limit: Option<usize>,
}

pub async fn outcome_history(
    State(state): State<AppState>,
    Query(query): Query<OutcomeHistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    if !matches!(query.collection.as_str(), "missions" | "work")
        || query
            .status
            .as_deref()
            .is_some_and(|s| !matches!(s, "failed" | "cancelled" | "timed-out" | "completed"))
        || query.limit.is_some_and(|limit| !(1..=200).contains(&limit))
        || query.until.is_some_and(|until| until < query.since)
        || query.since > i64::MAX as u64
        || query.until.is_some_and(|until| until > i64::MAX as u64)
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-history-filter",
            "select missions or work, a terminal status, a valid time window, and a limit of 1 through 200",
        )));
    }
    blocking_store(move || {
        state.store.read_snapshot(|index| {
            state.store.outcome_history(
                &query.collection,
                u128::from(query.since),
                query.until.map(u128::from).unwrap_or_else(client_now_ms),
                query.status.as_deref(),
                query.actor.as_deref(),
                query.before.unwrap_or(index.saturating_add(1)),
                query.limit.unwrap_or(50),
            )
        })
    })
    .await
    .map(Json)
}

pub async fn performance_report() -> Json<Value> {
    Json(crate::performance::snapshot())
}

pub async fn list_mission_runs(
    State(state): State<AppState>,
    Query(query): Query<MissionRunQuery>,
) -> Result<Json<Vec<MissionRunView>>, ApiError> {
    let store = state.store.clone();
    match (query.root, query.mission) {
        (Some(root), None) => blocking_store(move || {
            let mut runs = store.mission_runs_for_root(&root)?;
            annotate_stuck_gates(&store, &mut runs)?;
            Ok(runs)
        })
        .await
        .map(Json),
        (None, Some(mission)) => {
            blocking_store(move || store.active_mission_runs_for_mission(&mission))
                .await
                .map(Json)
        }
        _ => Err(ApiError::bad(St3Error::new(
            "invalid-mission-run-query",
            "select exactly one mission or root mission run",
        ))),
    }
}

pub async fn list_mission_run_tree(
    State(state): State<AppState>,
    Query(query): Query<MissionRunTreeQuery>,
) -> Result<Json<crate::model::MissionRunTreePage>, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=50).contains(&limit) {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-run-limit",
            "mission run tree page limit must be 1 through 50",
        )));
    }
    let store = state.store.clone();
    let page = blocking_store(move || {
        store.read_snapshot(|frontier| {
            let Some((mut runs, next_cursor)) =
                store.mission_runs_for_root_page(&query.root, query.after.as_deref(), limit)?
            else {
                return Ok(None);
            };
            annotate_stuck_gates(&store, &mut runs)?;
            Ok(Some(crate::model::MissionRunTreePage {
                runs,
                has_more: next_cursor.is_some(),
                next_cursor,
                frontier,
            }))
        })
    })
    .await?;
    page.map(Json).ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "invalid-mission-run-cursor",
            "the mission run cursor is absent from this root; start again without a cursor",
        ))
    })
}

fn annotate_stuck_gates(store: &Store, runs: &mut [MissionRunView]) -> anyhow::Result<()> {
    let active = runs
        .iter()
        .filter(|run| matches!(run.status.as_str(), "running" | "standing" | "blocked"))
        .map(|run| run.subject.clone())
        .collect::<Vec<_>>();
    let missions = store.mission_specs_for_runs(&active)?;
    for run in runs {
        if let Some(mission) = missions.get(&run.subject) {
            run.stuck_gates = crate::reconcile::stuck_field_gates(store, run, mission)?;
        }
    }
    Ok(())
}

pub async fn get_mission_run(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<MissionRunView>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    blocking_store(move || {
        let Some(mut run) = store.mission_run(&run_for_read)? else {
            return Ok(None);
        };
        annotate_stuck_gates(&store, std::slice::from_mut(&mut run))?;
        Ok(Some(run))
    })
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))
}

pub async fn revise_mission_run(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
    Json(request): Json<MissionRevisionRequest>,
) -> Result<Json<RevisionSubmissionView>, ApiError> {
    if let Some(cached) = cached_revision_submission(
        &state,
        &format!("{}:adopt", request.idempotency_key),
        &format!("{}:propose", request.idempotency_key),
    )? {
        return Ok(Json(cached));
    }
    let current = state
        .store
        .mission_run(&run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))?;
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    if crate::mission::top_level_mission_ids(&initial.missions).len() != 1 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-revision-intent",
            "a run revision must contain exactly one top-level mission",
        )));
    }
    if !initial.subjects.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-revision-intent",
            "a run revision can contain only its mission",
        )));
    }
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let mission_id = current
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&current.mission);
    let Some(replacement) = intent.missions.get(mission_id) else {
        return Err(ApiError::bad(St3Error::new(
            "wrong-mission-revision",
            format!("revision does not replace mission `{mission_id}`"),
        )));
    };
    let old = state
        .store
        .mission_spec(mission_id, Some(&current.revision))
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-mission-revision",
                "the current mission revision is unavailable",
            ))
        })?;
    if old.inputs != replacement.inputs {
        return Err(ApiError::bad(St3Error::new(
            "run-input-mutation",
            "a run revision cannot change its input declarations",
        )));
    }
    let actor = if request.actor.contains('/') {
        request.actor.clone()
    } else {
        format!("agent/{}", request.actor)
    };
    let (_, reviewers) =
        crate::store::analyze_mission_revision(&old, replacement, &current.requester)
            .map_err(ApiError::bad)?;
    state
        .store
        .validate_revision_seat_carry(&current, replacement)
        .map_err(ApiError::bad)?;
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
            "mission-revision-blocked",
            planned.blockers.join("; "),
        )));
    }
    // The revision records its publisher: a broken gate in it raises attention for them.
    state
        .store
        .apply_as(
            &publication,
            &planned.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
            Some(&actor),
        )
        .map_err(ApiError::bad)?;
    // A failed run has no active work to drain, so it adopts an unreviewed revision now.
    let reopening = current.status == "failed" && current.phase == "terminal";
    let mut revised = if reviewers.is_empty()
        && (reopening || matches!(old.revision_cutover, RevisionCutover::RestartActive))
    {
        let mission_run = state
            .store
            .adopt_mission_revision(
                &run,
                replacement,
                &actor,
                &request.reason,
                &format!("{}:adopt", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        RevisionSubmissionView {
            provenance: None,
            status: "applied".into(),
            mission_run,
            proposal: None,
        }
    } else {
        let proposal = state
            .store
            .create_revision_proposal(
                &run,
                replacement,
                &actor,
                &request.reason,
                &format!("{}:propose", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        RevisionSubmissionView {
            provenance: None,
            status: proposal.status.clone(),
            mission_run: state
                .store
                .mission_run(&run)
                .map_err(ApiError::internal)?
                .expect("the revised mission run exists"),
            proposal: Some(proposal),
        }
    };
    revised.provenance = state
        .store
        .mission_provenance(mission_id, &replacement.revision)
        .map_err(ApiError::internal)?;
    signal_changed(&state);
    Ok(Json(revised))
}

pub async fn list_run_generations(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<Vec<RunGenerationView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.run_generations(&run))
        .await
        .map(Json)
}

pub async fn get_run_generation(
    State(state): State<AppState>,
    AxumPath(generation): AxumPath<String>,
) -> Result<Json<RunGenerationView>, ApiError> {
    let store = state.store.clone();
    let generation_for_read = generation.clone();
    blocking_store(move || store.run_generation(&generation_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("run generation `{generation}` does not exist")))
}

pub async fn get_run_revision_proposal(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<RevisionProposalView>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    blocking_store(move || store.revision_proposal_for_run(&run_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "mission run `{run}` has no pending revision proposal"
            ))
        })
}

/// A person or an agent sets a finished run's outcome.
pub async fn set_mission_run_outcome(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
    Json(request): Json<MissionRunOutcomeRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    if let Some(cached) = state
        .store
        .cached_idempotency_response::<MissionRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(cached));
    }
    let actor = person_or_agent_actor(&request.actor, "run-outcome-authority-denied")?;
    let current = state
        .store
        .mission_run(&run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))?;
    let store = state.store.clone();
    let outcome = blocking_action(move || {
        store.set_mission_run_outcome(
            &current.subject,
            &request.status,
            &actor,
            &request.reason,
            &request.idempotency_key,
        )
    })
    .await?;
    signal_changed(&state);
    Ok(Json(outcome))
}

/// A person or the run's requester changes who a running run reports to.
pub async fn set_mission_run_report(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
    Json(request): Json<MissionRunReportRequest>,
) -> Result<Json<MissionRunReportView>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "run-report-authority-denied")?;
    let store = state.store.clone();
    let report = blocking_action(move || {
        store.set_mission_run_report(
            &run,
            &actor,
            request.report_to.as_deref(),
            request.stalled_after_ms,
            request.report_stalls,
            request.report_completed,
            &request.idempotency_key,
        )
    })
    .await?;
    signal_changed(&state);
    Ok(Json(report))
}

fn stage_eval_documents(
    state: &AppState,
    workspace: &Path,
    intent: &crate::model::NormalizedIntent,
) -> Result<(), St3Error> {
    for reference in &intent.document_refs {
        let (name, hash) = reference.rsplit_once('@').ok_or_else(|| {
            St3Error::new(
                "invalid-document-reference",
                format!("eval document reference `{reference}` has no hash"),
            )
        })?;
        if state
            .store
            .get_document(name, hash)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
            .is_some()
        {
            continue;
        }
        let path = workspace.join(".st3-documents").join(hash);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            St3Error::new(
                "missing-eval-document",
                format!(
                    "eval document `{reference}` is absent from the store and {} is not staged: {error}",
                    path.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(St3Error::new(
                "invalid-eval-document",
                format!(
                    "staged eval document {} is not a regular file",
                    path.display()
                ),
            ));
        }
        let bytes = fs::read(&path).map_err(|error| {
            St3Error::new(
                "invalid-eval-document",
                format!("read staged eval document {}: {error}", path.display()),
            )
        })?;
        if hex::encode(Sha256::digest(&bytes)) != hash {
            return Err(St3Error::new(
                "eval-document-hash-mismatch",
                format!(
                    "staged eval document {} does not match `{reference}`",
                    path.display()
                ),
            ));
        }
        let expected = state
            .store
            .latest_document_token(name)
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        state.store.put_document(
            name,
            &bytes,
            &expected,
            &format!("eval-document:{name}:{hash}"),
        )?;
    }
    Ok(())
}
