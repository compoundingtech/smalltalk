use super::*;
#[derive(Deserialize)]
pub(super) struct LaunchQuery {
    subject: String,
    token: String,
}

pub(super) async fn get(
    State(state): State<AppState>,
    AxumPath(host): AxumPath<String>,
    Query(query): Query<LaunchQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let host = host.trim_start_matches("host/");
    if host == state.node {
        let store = state.store.clone();
        return blocking_store(move || {
            crate::agent_launch::read(&store, &query.subject, &query.token)
                .and_then(|status| Ok(serde_json::to_value(status)?))
        })
        .await
        .map(Json);
    }
    let host_id = client_host_id(host);
    let actor = headers
        .get("x-st3-person")
        .and_then(|value| value.to_str().ok())
        .filter(|actor| actor.starts_with("person/") || actor.starts_with("agent/"))
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-actor",
                "remote launch observation needs a concrete actor",
            ))
        })?;
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
                },
            },
        )
        .await
        .map(Json)
        .map_err(|error| remote_read_error(&host_id, error))
}
