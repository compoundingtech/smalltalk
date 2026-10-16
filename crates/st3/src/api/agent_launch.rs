use super::*;
// Coalesce the shared daemon feed without polling when it is quiet. A waiter captures
// at most once per 500 ms after its initial read, even during unrelated event storms.
const CAPTURE_SPACING: Duration = Duration::from_millis(500);
#[derive(Deserialize)]
pub(super) struct LaunchQuery {
    subject: String,
    token: String,
    after: Option<String>,
    #[serde(default)]
    wait_ms: u64,
}

pub(super) async fn get(
    State(state): State<AppState>,
    AxumPath(host): AxumPath<String>,
    Query(query): Query<LaunchQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    // Retain the concrete actor across both the Unix and authenticated peer boundaries.
    // This is not a per-agent ownership check; the route's reader policy needs cover.
    let actor = headers
        .get("x-st3-person")
        .and_then(|value| value.to_str().ok())
        .filter(|actor| {
            !actor.chars().any(char::is_whitespace)
                && (actor.starts_with("person/")
                    && actor.matches('/').count() == 1
                    && actor.len() > "person/".len()
                    || actor.starts_with("agent/")
                        && actor.split('/').all(|part| {
                            !part.is_empty()
                                && part
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                        }))
        })
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-actor",
                "launch observation needs a concrete actor",
            ))
        })?;
    let host = host.trim_start_matches("host/");
    if query
        .after
        .as_ref()
        .is_some_and(|after| after.len() != 64 || !after.bytes().all(|b| b.is_ascii_hexdigit()))
        || query.wait_ms > 10_000
    {
        return Err(ApiError::bad(St3Error::new(
            "validation-failed",
            "invalid launch wait marker or wait budget",
        )));
    }
    if host == state.node {
        // Register before capturing. A commit between the read and await stays visible.
        let mut changed = state.event_notify.subscribe();
        let store = state.store.clone();
        let subject = query.subject.clone();
        let token = query.token.clone();
        let mut status =
            blocking_store(move || crate::agent_launch::read(&store, &subject, &token)).await?;
        let marker = crate::agent_launch::wait_marker(&status).map_err(ApiError::internal)?;
        if query.wait_ms == 0 || query.after.as_deref() != Some(marker.as_str()) {
            return serde_json::to_value(status)
                .map(Json)
                .map_err(ApiError::internal);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(query.wait_ms);
        let mut next_capture = tokio::time::Instant::now() + CAPTURE_SPACING;
        loop {
            // Unchanged notifications do not end the request. No database snapshot is
            // held across either await; a quiet feed causes no additional captures.
            match tokio::time::timeout_at(deadline, changed.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
            tokio::time::sleep_until(next_capture.min(deadline)).await;
            changed.borrow_and_update();
            let store = state.store.clone();
            let subject = query.subject.clone();
            let token = query.token.clone();
            status =
                blocking_store(move || crate::agent_launch::read(&store, &subject, &token)).await?;
            let marker = crate::agent_launch::wait_marker(&status).map_err(ApiError::internal)?;
            if query.after.as_deref() != Some(marker.as_str())
                || tokio::time::Instant::now() >= deadline
            {
                break;
            }
            next_capture = tokio::time::Instant::now() + CAPTURE_SPACING;
        }
        return serde_json::to_value(status)
            .map(Json)
            .map_err(ApiError::internal);
    }
    let host_id = client_host_id(host);
    let relay = state
        .client_relay
        .as_ref()
        .filter(|relay| relay.reaches(&host_id))
        .ok_or_else(|| remote_unavailable_for_owner(&state, &host_id))?;
    relay
        .read(
            &host_id,
            &crate::peer::ClientReadRequest {
                authority_actor: actor.into(),
                relay: None,
                request: crate::peer::ClientReadOperation::AgentLaunch {
                    subject: query.subject,
                    token: query.token,
                    after: query.after,
                    wait_ms: query.wait_ms,
                },
            },
        )
        .await
        .map(Json)
        .map_err(|error| remote_read_error(&host_id, error))
}
