use super::*;

pub async fn available_harnesses() -> Result<Json<Vec<String>>, ApiError> {
    let harnesses = read_deadline::spawn_blocking(crate::environment::available_harnesses)
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    Ok(Json(harnesses))
}

pub async fn doctor(State(state): State<AppState>) -> Result<Json<DoctorReport>, ApiError> {
    let reader_store = state.store.clone();
    let environment = crate::api::read_deadline::spawn_blocking(crate::environment::snapshot)
        .await
        .map_err(ApiError::internal)?;
    // Linking a crate takes seconds, so it runs while the other checks do.
    let build_tools = environment.as_ref().ok().cloned().map(|environment| {
        crate::api::read_deadline::spawn_blocking(move || crate::environment::check_build_tools(&environment))
    });
    let pty_root = state.pty_root.clone();
    let priority = crate::api::read_deadline::spawn_blocking(move || {
        let observations = st_runtime::PtyRuntime::new(pty_root)
            .snapshot()
            .unwrap_or_default();
        st_runtime::priority_report(&observations)
    });
    let token = crate::resource::github_auth().await;
    let writer_wait = smallclaims::sqlite::telemetry::current_request();
    let mut report = crate::api::read_deadline::spawn_blocking(move || {
        // Sealing this node's batches writes through the store's writer, so the request's
        // writer accounting must follow the job off the runtime thread.
        let _writer_scope = smallclaims::sqlite::telemetry::enter_request(writer_wait);
        // This node's claims are signed as their batches are sealed; seal and judge them so
        // the signature counts cover everything written so far.
        state.store.replication_snapshot().map_err(ApiError::internal)?;
        doctor_report(&state)
    })
        .await
        .map_err(ApiError::internal)??
        .0;
    if let Some(build_tools) = build_tools {
        report.checks.push(build_tools_check(
            &build_tools.await.map_err(ApiError::internal)?,
        ));
    }
    report.checks.push(match environment {
        Ok(environment) => DoctorCheck {
            name: "daemon-environment".into(),
            status: "pass".into(),
            message: format!(
                "account interactive login shell; refreshed on use every 60 seconds; PATH={}",
                environment.get("PATH").map(String::as_str).unwrap_or("")
            ),
        },
        Err(error) => DoctorCheck {
            name: "daemon-environment".into(),
            status: "fail".into(),
            message: error.to_string(),
        },
    });
    let (status, message) = priority.await.map_err(ApiError::internal)?;
    report.checks.push(DoctorCheck {
        name: "priority".into(),
        status: status.into(),
        message,
    });
    report.checks.push(DoctorCheck {
        name: "github-observer-auth".into(),
        status: if token.is_ok() { "pass" } else { "warn" }.into(),
        message: match token {
            Ok(_) => "GitHub observers have a credential source; credential values are not displayed".into(),
            Err(error) => error.to_string(),
        },
    });
    // Request samples are live operational telemetry. Keep them on /v1/doctor,
    // outside the store-index-fenced operation projection built by doctor_report.
    let mut routes = request_latency_snapshot();
    routes.sort_by_key(|route| std::cmp::Reverse(route["p99_ms"].as_u64().unwrap_or_default()));
    for route in routes.into_iter().take(10) {
        report.checks.push(DoctorCheck {
            name: format!(
                "request-latency/{}",
                route["route"].as_str().unwrap_or("unknown")
            ),
            status: "pass".into(),
            message: format!(
                "{} requests; recent p50 {} ms, p99 {} ms, max {} ms",
                route["count"], route["p50_ms"], route["p99_ms"], route["max_ms"]
            ),
        });
    }
    for (name, status, message) in crate::slo::doctor_lines(&request_latency_windows()) {
        report.checks.push(DoctorCheck {
            name,
            status: status.into(),
            message,
        });
    }
    // Only a daemon that runs the divergence check knows the count; the rest show no line.
    if let Some(open) = reader_store.open_divergences_at_last_check() {
        let (name, status, message) = crate::slo::divergence_line(open);
        report.checks.push(DoctorCheck {
            name,
            status: status.into(),
            message,
        });
    }
    report.checks.extend(github_usage_checks(
        &crate::resource::github_usage_report(),
        client_now_ms(),
    ));
    report.checks.extend(crate::github_admission::doctor_lines(client_now_ms()));
    report.checks.push(reader_store.mailbox_wake_health());
    report.checks.push(descriptor_check());
    report.checks.push(reader_memory_check(
        reader_store.readers.usage(),
        smallclaims::sqlite::read_cache_kib(),
        smallclaims::sqlite::max_idle_read_connections(),
        smallclaims::sqlite::max_read_workers(),
        crate::memory::service_memory(),
    ));
    report.status = if report.checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if report.checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "pass"
    }
    .into();
    Ok(Json(report))
}

fn descriptor_check() -> DoctorCheck {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return DoctorCheck {
            name: "file-descriptors".into(),
            status: "warn".into(),
            message: format!(
                "cannot read descriptor limits: {}",
                std::io::Error::last_os_error()
            ),
        };
    }
    #[cfg(target_os = "linux")]
    let path = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let path = "/dev/fd";
    // Enumerating descriptors temporarily opens one itself.
    let usage = std::fs::read_dir(path)
        .ok()
        .map(|entries| entries.count().saturating_sub(1) as u64);
    descriptor_usage_check(limit.rlim_cur, limit.rlim_max, usage)
}

fn build_tools_check(tools: &crate::environment::BuildTools) -> DoctorCheck {
    use crate::environment::LinkResult;
    let mut problems = Vec::new();
    if !tools.missing.is_empty() {
        problems.push(format!(
            "missing from the login PATH: {}",
            tools.missing.join(", ")
        ));
    }
    match &tools.link {
        LinkResult::Linked => {}
        LinkResult::NotAttempted => {
            problems.push("no small crate was linked because cargo or rustc is missing".into());
        }
        LinkResult::Failed(error) => problems.push(format!("a small crate did not link: {error}")),
    }
    if problems.is_empty() {
        return DoctorCheck {
            name: "build-tools".into(),
            status: "pass".into(),
            message: format!(
                "{} are on the login PATH, and a small crate links",
                tools.found.join(", ")
            ),
        };
    }
    DoctorCheck {
        name: "build-tools".into(),
        status: "warn".into(),
        message: format!(
            "{}; install what is missing, or export its directory from the account's shell startup files",
            problems.join("; ")
        ),
    }
}

pub async fn operational_repair_plan(
    State(state): State<AppState>,
) -> Result<Json<OperationalRepairPlan>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.operational_repair_plan())
        .await
        .map(Json)
}

pub async fn apply_operational_repair(
    State(state): State<AppState>,
    Json(request): Json<OperationalRepairApplyRequest>,
) -> Result<Json<OperationalRepairResult>, ApiError> {
    let store = state.store.clone();
    let result = blocking_action(move || store.apply_operational_repair(&request.token)).await?;
    if result.applied != 0 {
        signal_changed(&state);
    }
    Ok(Json(result))
}

/// Spool one snapshot on a read worker; a slow downloader never holds the SQLite snapshot.
pub async fn backup_export(State(state): State<AppState>) -> Result<Response, ApiError> {
    let store = state.store.clone();
    let bodies = message_body::bodies(&state.state_dir);
    let file = blocking_store(move || {
        let mut file = tempfile::tempfile()?;
        store.write_backup_with_bodies(&mut file, Some(&bodies))?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    })
    .await?;
    let file = tokio::fs::File::from_std(file);
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut bytes = vec![0_u8; 64 * 1024];
        let read = file.read(&mut bytes).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        bytes.truncate(read);
        Ok(Some((bytes, file)))
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct UsageQuery {
    since_ms: Option<u64>,
    until_ms: Option<u64>,
}

pub async fn get_coordination_counts(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    let until = query.until_ms.unwrap_or(client_now_ms() as u64);
    let since = query.since_ms.unwrap_or(until.saturating_sub(86_400_000));
    if since > until {
        return Err(ApiError::bad(St3Error::new(
            "invalid-usage-period",
            "usage start must be before its end",
        )));
    }
    Ok(Json(
        blocking_store(move || state.store.coordination_counts(since, until)).await?,
    ))
}

pub async fn get_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    let until_ms = query.until_ms.unwrap_or(client_now_ms() as u64);
    let since_ms = query
        .since_ms
        .unwrap_or(until_ms.saturating_sub(86_400_000));
    if since_ms > until_ms {
        return Err(ApiError::bad(St3Error::new(
            "invalid-usage-period",
            "usage start must be before its end",
        )));
    }
    let store = state.store.clone();
    let (rows, agent_messages, limits) = blocking_store(move || {
        let (rows, agent_messages) = store.usage_period_report(since_ms, until_ms)?;
        Ok((
            rows,
            agent_messages,
            store.account_limits()?,
        ))
    })
    .await?;
    let mut report = json!({"since_ms": since_ms, "until_ms": until_ms, "rows": rows, "limits": limits, "agent_messages": agent_messages});
    // The policy as the config file says now, so `st usage` shows which accounts it never stops.
    let reloaded = tokio::task::spawn_blocking(crate::config::reload_daemon_limits)
        .await
        .ok()
        .flatten();
    let policy = reloaded
        .as_ref()
        .and_then(|reloaded| reloaded.as_ref().ok())
        .and_then(|config| crate::store::LimitsPolicy::from_config(config).map(|policy| (config, policy)));
    // A reading older than the limits cadence is the last known one, not the current one.
    let fresh_ms = policy
        .as_ref()
        .map_or(crate::store::ACCOUNT_READING_WINDOW_MS, |(_, policy)| policy.fresh_ms);
    let now = client_now_ms() as u64;
    for (row, limit) in report["limits"]
        .as_array_mut()
        .into_iter()
        .flatten()
        .zip(&limits)
    {
        row["stale"] = json!(now.saturating_sub(limit.measured_at_unix_ms) > fresh_ms);
    }
    if let Some((config, policy)) = policy {
        for (row, limit) in report["limits"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .zip(&limits)
        {
            row["exempt"] = json!(policy.exempts(limit));
        }
        report["limits_policy"] = json!({
            "stop_at_weekly_percent": config.stop_at_weekly_percent,
            "exempt_accounts": config.exempt_accounts,
            "exempt_harnesses": config.exempt_harnesses,
        });
    }
    Ok(Json(report))
}

pub async fn get_slo(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let rows = blocking_store(move || slo_rows(&store)).await?;
    Ok(Json(json!({"as_of_unix_ms": client_now_ms() as u64, "rows": rows})))
}

/// What was left of one objective or budget across a period, from the local history log.
pub async fn get_slo_history(
    State(state): State<AppState>,
    Query(query): Query<SloHistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let until_ms = query.until_ms.unwrap_or(client_now_ms() as u64);
    let since_ms = query
        .since_ms
        .unwrap_or(until_ms.saturating_sub(crate::slo_history::RETENTION_MS / 7));
    let steps = query.steps.unwrap_or(120);
    if since_ms >= until_ms || until_ms - since_ms > crate::slo_history::RETENTION_MS || !(1..=1_000).contains(&steps) {
        return Err(ApiError::bad(St3Error::new(
            "invalid-slo-period",
            "the period must end after it starts, span at most 7 days, and use 1 to 1000 steps",
        )));
    }
    let dir = crate::slo_history::dir(&state.state_dir);
    let id = query.id.clone();
    let points = tokio::task::spawn_blocking(move || crate::slo_history::read(&dir, &id, since_ms, until_ms))
        .await
        .map_err(|error| ApiError::bad(St3Error::new("slo-history-unavailable", error.to_string())))?
        .map_err(|error| ApiError::bad(St3Error::new("slo-history-unavailable", error.to_string())))?;
    Ok(Json(json!({
        "id": query.id, "since_ms": since_ms, "until_ms": until_ms, "samples": points.len(),
        "steps": crate::slo_history::buckets(&points, since_ms, until_ms, steps),
    })))
}

#[derive(Deserialize)]
pub struct HarnessDiagnosticRequest {
    actor: String,
    code: String,
    reason: String,
    severity: String,
    status: String,
    incarnation_id: Option<String>,
    idempotency_key: String,
}

pub async fn post_harness_diagnostic(
    State(state): State<AppState>,
    Json(request): Json<HarnessDiagnosticRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = if request.actor.starts_with("agent/") {
        request.actor
    } else if !request.actor.contains('/') && !request.actor.trim().is_empty() {
        format!("agent/{}", request.actor)
    } else {
        return Err(ApiError::bad(St3Error::new(
            "invalid-diagnostic-actor",
            "a harness diagnostic requires one concrete agent subject",
        )));
    };
    if !matches!(request.severity.as_str(), "warning" | "error") {
        return Err(ApiError::bad(St3Error::new(
            "invalid-diagnostic-severity",
            "a harness diagnostic severity must be warning or error",
        )));
    }
    if request.code.trim().is_empty() || request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-harness-diagnostic",
            "a harness diagnostic requires a nonempty code and reason",
        )));
    }
    let mut fields = BTreeMap::from([
        ("code".into(), Value::String(request.code)),
        ("reason".into(), Value::String(request.reason)),
        ("severity".into(), Value::String(request.severity)),
        ("status".into(), Value::String(request.status)),
    ]);
    if let Some(incarnation) = request.incarnation_id {
        fields.insert("incarnation_id".into(), Value::String(incarnation));
    }
    let (record, appended) = state
        .store
        .append_claim_outcome(&ClaimInput {
            subject: actor.clone(),
            kind: "harness.diagnostic".into(),
            actor: Some(actor),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key),
        })
        .map_err(ApiError::bad)?;
    if appended {
        signal_changed(&state);
    }
    Ok(Json(record))
}

#[derive(Deserialize)]
pub struct StatusQuery {
    subject: Option<String>,
    owner_run: Option<String>,
    at_index: Option<u64>,
    #[serde(default)]
    history: bool,
    /// Only the subject's current harness; see `Store::status_harness_only`.
    #[serde(default)]
    harness_only: bool,
}

/// One subject's desired record. Each Claude seat's status line reads it on every render (every
/// five seconds), which a full status reduction of the seat made a tenth of a core on a member.
pub async fn get_desired(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<crate::model::DesiredSubject>, ApiError> {
    let store = state.store.clone();
    let named = subject.clone();
    blocking_store(move || store.desired_subjects_named(&[named]))
        .await?
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("`{subject}` has no desired record")))
}

pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || {
        if let (true, false, Some(subject)) = (query.harness_only, query.history, &query.subject) {
            store.status_harness_only(subject)
        } else if query.history {
            store.status_history(
                query.subject.as_deref(),
                query.owner_run.as_deref(),
                query.at_index,
            )
        } else {
            store.status_at(
                query.subject.as_deref(),
                query.owner_run.as_deref(),
                query.at_index,
            )
        }
    })
    .await
    .map(Json)
}

pub async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Result<Response, ApiError> {
    // The old array shape has no continuation field. Keep its bounded answer and expose
    // truncation explicitly in headers; owner traversal requires the new page envelope.
    if query.owner_run.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "event-pagination-required",
            "update the st CLI to this daemon version; owner-filtered traversal uses /v1/events/page",
        )));
    }
    if query.subject.is_some() {
        let store = state.store.clone();
        if blocking_store(move || store.event_payload_migration_pending()).await? {
            return Err(ApiError::bad(St3Error::new(
                "event-pagination-required",
                "update the st CLI to this daemon version; subject-filtered traversal during migration uses /v1/events/page",
            )));
        }
    }
    let mut changed = state.event_notify.subscribe();
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(query.timeout_ms.unwrap_or(30_000).clamp(10, 30_000));
    loop {
        let page = read_event_page(&state, &query).await?;
        if page.next_after.is_some()
            || query.wait == Some(false)
            || tokio::time::Instant::now() >= deadline
        {
            let next = page.next_after.unwrap_or(query.after).to_string();
            return Ok((
                [
                    ("x-st-next-after", next),
                    ("x-st-has-more", page.has_more.to_string()),
                    ("x-st-frontier", page.frontier.to_string()),
                ],
                Json(page.items),
            )
                .into_response());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if !matches!(
            tokio::time::timeout(remaining, changed.changed()).await,
            Ok(Ok(()))
        ) {
            let page = read_event_page(&state, &query).await?;
            return Ok((
                [
                    (
                        "x-st-next-after",
                        page.next_after.unwrap_or(query.after).to_string(),
                    ),
                    ("x-st-has-more", page.has_more.to_string()),
                    ("x-st-frontier", page.frontier.to_string()),
                ],
                Json(page.items),
            )
                .into_response());
        }
    }
}
