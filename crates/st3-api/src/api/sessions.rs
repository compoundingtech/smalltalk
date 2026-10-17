use super::*;

#[derive(Default, Deserialize)]
pub struct SessionListQuery {
    #[serde(default)]
    history: bool,
}

pub async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<Vec<crate::model::SubjectStatus>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.terminal_statuses(query.history))
        .await
        .map(Json)
}

#[derive(Deserialize)]
pub struct SessionLogQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_log_limit")]
    limit: usize,
    #[serde(default)]
    previous: bool,
    #[serde(default)]
    wait: bool,
}

pub async fn logs_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Query(query): Query<SessionLogQuery>,
) -> Result<Json<SessionLogChunk>, ApiError> {
    if query.limit == 0 || query.limit > 64 * 1024 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-log-limit",
            "a log chunk limit must be between 1 and 65536 bytes",
        )));
    }
    let session = read_deadline::query(&state.store, "/v1/sessions/logs/{*subject}", || {
        live_session(&state, &subject, None)
    })?;
    if session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal sessions expose a screen instead of an exec log",
        )));
    }
    let runtime =
        st_runtime::ExecRuntime::new(state.state_dir.join("exec"), state.state_dir.join("logs"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut generation = if query.previous {
            runtime
                .previous_generation(&session.runtime_id)
                .map_err(ApiError::internal)?
        } else {
            match runtime
                .observe(&session.runtime_id)
                .map_err(ApiError::internal)?
            {
                Some(st_runtime::ExecObservation::Running(generation))
                | Some(st_runtime::ExecObservation::Exited(generation)) => Some(generation),
                Some(st_runtime::ExecObservation::Indeterminate(reason)) => {
                    return Err(ApiError::internal(reason));
                }
                None => None,
            }
        }
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "subject `{subject}` has no {}exec log generation",
                if query.previous { "previous " } else { "" }
            ))
        })?;
        if !query.previous && generation.generation_id != session.incarnation_id {
            return Err(ApiError::bad(St3Error::new(
                "stale-incarnation",
                format!("subject `{subject}` changed exec generation"),
            )));
        }
        let log = runtime
            .read_log_bytes(&session.runtime_id, query.previous)
            .map_err(ApiError::internal)?
            .unwrap_or_default();
        let start = usize::try_from(query.after)
            .unwrap_or(usize::MAX)
            .min(log.len());
        let end = start.saturating_add(query.limit).min(log.len());
        let running = if query.previous {
            false
        } else {
            match runtime
                .observe(&session.runtime_id)
                .map_err(ApiError::internal)?
            {
                Some(st_runtime::ExecObservation::Running(latest)) => {
                    generation = latest;
                    true
                }
                Some(st_runtime::ExecObservation::Exited(latest)) => {
                    generation = latest;
                    false
                }
                Some(st_runtime::ExecObservation::Indeterminate(reason)) => {
                    return Err(ApiError::internal(reason));
                }
                None => false,
            }
        };
        if end > start || !query.wait || !running || tokio::time::Instant::now() >= deadline {
            return Ok(Json(SessionLogChunk {
                subject,
                runtime_id: session.runtime_id,
                generation_id: generation.generation_id,
                previous: query.previous,
                start_offset: start as u64,
                next_offset: end as u64,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&log[start..end]),
                eof: end == log.len() && !running,
                status: if running { "running" } else { "exited" }.into(),
                exit_code: generation.exit_code,
                exit_signal: generation.exit_signal,
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn screen_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<SessionScreen>, ApiError> {
    let session = live_session(&state, &subject, None)?;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "an exec session has a log instead of a terminal screen",
        )));
    }
    let screen = daemon_pty(&state)
        .and_then(|runtime| runtime.screen(&session.runtime_id))
        .map_err(ApiError::internal)?;
    Ok(Json(SessionScreen {
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: session.incarnation_id,
        screen,
    }))
}

pub async fn input_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<SessionInputRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    input_session_as(&state, subject, request, "requester").await
}

pub async fn clear_context(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<ContextClearRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    let result_key = format!(
        "session-control-result:context-clear:{subject}:{}",
        request.idempotency_key
    );
    if let Some(result) = state
        .store
        .idempotent_claim(&result_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(session_control_response(&subject, &result)));
    }
    let session = live_session(&state, &subject, Some(&request.expected_incarnation))?;
    if !session.terminal || session.driver.as_deref() != Some("claude") {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "context clear requires a terminal Claude driver in st v1",
        )));
    }
    let request_key = format!(
        "session-control-request:context-clear:{subject}:{}",
        request.idempotency_key
    );
    if let Some(prior) = state
        .store
        .operation_claim(&request_key)
        .map_err(ApiError::internal)?
    {
        return finish_session_control(
            &state,
            &subject,
            "harness.context-clear.result",
            &result_key,
            &prior,
            &session,
            Ok(()),
        );
    }
    let request_claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.context-clear.requested".into(),
            actor: Some("requester".into()),
            fields: BTreeMap::from([
                (
                    "runtime_id".into(),
                    Value::String(session.runtime_id.clone()),
                ),
                (
                    "incarnation_id".into(),
                    Value::String(session.incarnation_id.clone()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request_key),
        })
        .map_err(ApiError::bad)?;
    let effect_state = state.clone();
    let effect_session = session.clone();
    let effect = blocking_external(move || daemon_pty(&effect_state).and_then(|runtime| {
        runtime.send_line_if(&effect_session.runtime_id, "/clear", Some(&effect_session.incarnation_id))
    })).await?;
    finish_session_control(
        &state,
        &subject,
        "harness.context-clear.result",
        &result_key,
        &request_claim,
        &session,
        effect,
    )
}

pub async fn signal_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<SessionSignalRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    signal_session_as(state, subject, request, "requester").await
}

pub async fn attach_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(_request): Json<AttachRequest>,
) -> Result<Json<Attachment>, ApiError> {
    let session = live_session(&state, &subject, None)?;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal attachment requires a terminal session",
        )));
    }
    // Issuing a capability waits for the store writer, which a reconcile pass can hold for
    // seconds; the wait must not hold a runtime worker.
    let store = state.store.clone();
    let (capability_subject, incarnation_id) = (subject.clone(), session.incarnation_id.clone());
    let (capability, expires_at_unix_ms) = blocking_store(move || {
        store.issue_capability(
            "terminal",
            &capability_subject,
            Some(&incarnation_id),
            30_000,
        )
    })
    .await?;
    Ok(Json(Attachment {
        websocket_path: format!(
            "/v1/sessions/terminal/{}?capability={}",
            urlencoding::encode(&subject),
            capability
        ),
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: Some(session.incarnation_id),
        capability,
        expires_at_unix_ms,
    }))
}

/// Name the PTY session of a running terminal on this host, so `st terminals attach` can connect
/// to it directly. It checks what an attachment checks but issues no capability: it only reads,
/// so it answers while a busy reconciler holds the writer, and the attach never waits for this
/// daemon again. The caller proves the incarnation against the PTY itself before attaching.
pub async fn local_terminal(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<LocalTerminal>, ApiError> {
    // A read can still wait on a saturated disk; it must not hold a runtime worker meanwhile.
    let (lookup, lookup_subject) = (state.clone(), subject.clone());
    let session = crate::api::read_deadline::spawn_blocking(move || live_session(&lookup, &lookup_subject, None))
        .await
        .map_err(ApiError::internal)??;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal attachment requires a terminal session",
        )));
    }
    // An isolated daemon may run with a relative PTY root; the client resolves nothing itself.
    let pty_root = std::path::absolute(&state.pty_root).map_err(ApiError::internal)?;
    Ok(Json(LocalTerminal {
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: session.incarnation_id,
        pty_root,
    }))
}

#[derive(Deserialize)]
pub struct TerminalQuery {
    capability: String,
}

pub async fn terminal_session(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Query(query): Query<TerminalQuery>,
) -> Result<Response, ApiError> {
    let store = state.store.clone();
    let secret = query.capability;
    let capability = blocking_action(move || store.consume_capability(&secret, "terminal")).await?;
    if capability.used || capability.subject != subject {
        return Err(ApiError::bad(St3Error::new(
            "invalid-capability",
            "the terminal capability is used or names another subject",
        )));
    }
    let status = state
        .store
        .status(Some(&subject))
        .map_err(ApiError::internal)?;
    let actual = status
        .subjects
        .first()
        .and_then(|item| item.actual.as_ref())
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    let fields = actual.get("fields").unwrap_or(actual);
    let runtime_id = fields
        .get("runtime_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no runtime")))?
        .to_owned();
    let current_incarnation = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if capability.incarnation_id != current_incarnation {
        return Err(ApiError::bad(St3Error::new(
            "stale-incarnation",
            "the terminal incarnation changed before attachment",
        )));
    }
    Ok(websocket
        .protocols(["st3.terminal.v1"])
        .on_upgrade(move |socket| terminal_proxy(socket, state.pty_root, runtime_id)))
}

async fn terminal_proxy(socket: WebSocket, pty_root: std::path::PathBuf, runtime_id: String) {
    let stream = match tokio::net::UnixStream::connect(pty_core::registry::socket_path_in(
        &pty_root,
        &runtime_id,
    ))
    .await
    {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let (mut writer, mut reader) = socket.split();
    let (mut output_stream, mut input_stream) = stream.into_split();
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut bytes = vec![0_u8; 8192];
    loop {
        tokio::select! {
            read = output_stream.read(&mut bytes) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) if writer
                        .send(WsMessage::Binary(bytes[..count].to_vec().into()))
                        .await
                        .is_err() => break,
                    Ok(_) => {}
                }
            }
            message = reader.next() => {
                let payload = match message {
                    Some(Ok(WsMessage::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(WsMessage::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => continue,
                };
                if input_stream.write_all(&payload).await.is_err() {
                    break;
                }
            }
        }
    }
}
