use super::*;
use axum::http::HeaderMap;

fn forbidden(message: &str) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: message.into(),
        details: Box::default(),
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NoteQuery {
    actor: Option<String>,
}

pub(super) async fn read(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<NoteQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let actor = session.authority_actor;
    if !(actor.starts_with("person/") || actor.starts_with("agent/"))
        || query.actor.as_deref().is_some_and(|requested| requested != actor)
    {
        return Err(forbidden("identify the person or agent reading its directive notes"));
    }
    let (snapshot, notes) = blocking_store(move || {
        state.store.read_snapshot(|index| {
            Ok((client_snapshot_at(&state, index), state.store.directive_notes(&actor)?))
        })
    }).await?;
    Ok((Extension(snapshot), Json(json!({"notes": notes}))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NoteWrite {
    person: String,
    actor: String,
    text: Option<String>,
    expires_at: Option<String>,
}

pub(super) async fn write(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NoteWrite>,
) -> Result<Json<Value>, ApiError> {
    // The legacy write surface is local/trusted, not a paired-client delegation.
    // Require the same concrete Unix identity used by authenticated client reads;
    // free-mode agents cannot turn a body field into person authority.
    if headers.contains_key(axum::http::header::AUTHORIZATION) {
        return Err(forbidden("only the person on the trusted local transport writes a directive note"));
    }
    let mut request = Request::builder().uri("/v1/notes").body(Body::empty())
        .map_err(ApiError::internal)?;
    *request.headers_mut() = headers;
    let session = client_v0::authenticate(&state, &request, "unix")?;
    if !session.authority_actor.starts_with("person/")
        || session.authority_actor != input.actor || input.actor != input.person
    {
        return Err(forbidden("only the note's person may set or clear it"));
    }
    let note = blocking_action(move || state.store.set_directive_note(
        &input.person, &input.actor, input.text.as_deref(), input.expires_at.as_deref(),
    )).await?;
    Ok(Json(json!({"note": note})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    async fn read_as(state: &AppState, actor: Option<&str>, requested: Option<&str>) -> (StatusCode, Value) {
        let path = requested.map_or_else(|| "/v1/client/notes".into(), |actor| {
            format!("/v1/client/notes?actor={}", urlencoding::encode(actor))
        });
        let mut request = Request::builder().uri(path);
        if let Some(actor) = actor { request = request.header("x-st3-person", actor); }
        let response = router(state.clone()).oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn note_read_requires_identified_actor_and_rejects_actor_switching() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let (status, _) = read_as(&state, None, Some("person/ada")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = read_as(&state, Some("agent/worker"), Some("person/ada")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, value) = read_as(&state, Some("person/ada"), None).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        assert_eq!(value["value"]["notes"], json!([]));
        let (status, _) = read_as(&state, Some("agent/unknown"), None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn note_write_cannot_impersonate_the_person_or_use_paired_authority() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        for actor in [None, Some("agent/worker"), Some("person/other")] {
            let mut request = Request::builder().method("PUT").uri("/v1/notes").header("content-type", "application/json");
            if let Some(actor) = actor { request = request.header("x-st3-person", actor); }
            let response = router(state.clone()).oneshot(request.body(Body::from(json!({
                "person":"person/ada", "actor":"person/ada", "text":"approved"
            }).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        let response = router(state).oneshot(Request::builder().method("PUT").uri("/v1/notes")
            .header("content-type", "application/json").header("x-st3-person", "person/ada")
            .header("authorization", "Bearer not-a-person")
            .body(Body::from(json!({"person":"person/ada", "actor":"person/ada", "text":null}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
