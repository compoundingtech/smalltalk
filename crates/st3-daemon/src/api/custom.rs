//! Custom schema administration and generic client reads; paired devices cannot register kinds.
use super::*;
use crate::store::custom::{RegistrationRequest, ReplyRequest};

pub(super) async fn register(
    State(state): State<AppState>,
    Json(request): Json<RegistrationRequest>,
) -> Result<Json<Value>, ApiError> {
    person_or_agent_actor(&request.actor, "invalid-custom-actor")?;
    let store = state.store.clone();
    let result = blocking_action(move || store.register_custom_kind(&request)).await?;
    signal_changed(&state);
    Ok(Json(result))
}
pub(super) async fn registrations(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"items":state.store.custom_registrations().map_err(ApiError::internal)?}),
    ))
}
pub(super) async fn reply(
    State(state): State<AppState>,
    Json(request): Json<ReplyRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    person_or_agent_actor(&request.actor, "invalid-custom-actor")?;
    let store = state.store.clone();
    let c = blocking_action(move || store.reply_custom_subject(&request)).await?;
    signal_changed(&state);
    Ok(Json(c))
}
#[derive(Default, Deserialize)]
pub(super) struct ListQuery {
    kind: Option<String>,
    version: Option<u32>,
    cursor: Option<String>,
    limit: Option<usize>,
}
pub(super) async fn list(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ListQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let cursor = query
        .cursor
        .as_ref()
        .map(|cursor| {
            cursor
                .strip_prefix("custom-page/")
                .and_then(|s| {
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(s)
                        .ok()
                })
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .ok_or_else(|| client_v0::validation("invalid custom page cursor"))
        })
        .transpose()?;
    let (snapshot, page) = blocking_store(move || {
        state.store.read_snapshot(|index| {
            if let Some(value) = &cursor
                && (value["index"] != index || value["kind"] != json!(query.kind)
                    || value["version"] != json!(query.version)
                    || value["limit"] != limit || value["host"] != state.node
                    || !value["after"].is_string()) {
                return Ok(None);
            }
            let after = cursor.as_ref().and_then(|v| v["after"].as_str());
            let mut items = state.store.custom_subjects(query.kind.as_deref(), query.version, after, limit + 1)?;
            let more = items.len() > limit;
            items.truncate(limit);
            let next = if more {
                Some(format!("custom-page/{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&json!({"host":state.node,"index":index,"kind":query.kind,"version":query.version,"limit":limit,"after":items.last().unwrap()["id"]}))?
                )))
            } else { None };
            let page = json!({"kind":"page","collection":"custom-subjects","filters":{},"items":items,"page":{"has_more":more,"next_cursor":next,"limit":limit}});
            Ok(Some((client_snapshot_at(&state, index), page)))
        })
    }).await?.ok_or_else(|| ApiError {
        status: StatusCode::GONE, code: "page-cursor-expired".into(),
        message: "custom collection changed; restart pagination".into(), details: Box::default(),
    })?;
    Ok((Extension(snapshot), Json(page)))
}
pub(super) async fn read(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let result = blocking_store(move || {
        state.store.read_snapshot(|index| {
            Ok(state
                .store
                .custom_subject(&id)?
                .map(|value| (client_snapshot_at(&state, index), value)))
        })
    })
    .await?;
    let (snapshot, value) =
        result.ok_or_else(|| ApiError::not_found("custom source is unavailable"))?;
    Ok((Extension(snapshot), Json(value)))
}
#[derive(Deserialize)]
pub(super) struct BasisQuery {
    subject: String,
    kinds: String,
}
pub(super) async fn basis(
    State(state): State<AppState>,
    Query(query): Query<BasisQuery>,
) -> Result<Json<Value>, ApiError> {
    let kinds = query
        .kinds
        .split(',')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if kinds.is_empty()
        || kinds.len() > 32
        || kinds.iter().any(|k| !st3_schema::is_custom_claim_kind(k))
    {
        return Err(client_v0::validation(
            "basis needs 1..32 custom claim kinds",
        ));
    }
    st3_schema::registry()
        .validate_subject(&query.subject)
        .map_err(|e| client_v0::validation(e.message))?;
    Ok(Json(
        json!({"subject":query.subject,"kinds":kinds,"revision":state.store.custom_basis_revision(&query.subject,&kinds).map_err(ApiError::internal)?}),
    ))
}
