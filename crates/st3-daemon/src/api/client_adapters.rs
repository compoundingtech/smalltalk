//! Enrolled provider transports: external attribution and resumable reply notifications.

use super::client_v0::{ClientSession, require_scope};
use super::*;

fn external_source(source: &str) -> bool {
    source.len() <= 512
        && source.starts_with("external/")
        && source.split('/').count() == 4
        && source.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

fn adapter_route(state: &AppState, actor: &str) -> Result<(Vec<String>, String), ApiError> {
    let declaration = state
        .store
        .latest_claim(actor, Some("intent.desired"))
        .map_err(ApiError::internal)?;
    let member = declaration.as_ref().map(|claim| &claim.body["member"]);
    let route = member
        .and_then(|member| {
            (actor.starts_with("agent/") && member["kind"] == "agent" && member["driver"].is_null())
                .then(|| {
                    (
                        member["tags"]["st3.adapter.sources"]
                            .as_str()
                            .or_else(|| member["tags"]["st3.adapter.source"].as_str()),
                        member["tags"]["st3.adapter.target"].as_str(),
                    )
                })
        })
        .and_then(|(sources, target)| sources.zip(target));
    let Some((sources, target)) = route else {
        return Err(ApiError::bad(St3Error::new(
            "adapter-route-refused",
            "an adapter requires an enrolled program seat",
        )));
    };
    let sources: Vec<String> = sources.split(',').map(str::to_owned).collect();
    if sources.is_empty()
        || sources.len() > 16
        || sources.iter().any(|source| !external_source(source))
        || sources
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != sources.len()
        || !target.starts_with("agent/")
    {
        return Err(ApiError::bad(St3Error::new(
            "adapter-route-refused",
            "enrollment requires up to sixteen distinct external accounts and one agent destination",
        )));
    }
    Ok((sources, target.to_owned()))
}

#[derive(Deserialize)]
pub(super) struct DeliveryQuery {
    #[serde(default)]
    after: u64,
    #[serde(default)]
    wait_ms: u64,
}

/// Interim bounded watch: a durable local graph frontier, not a provider receipt.
/// Subscribe before reading so a reply between the read and wait is never lost.
pub(super) async fn deliveries(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<DeliveryQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "control.messages")?;
    let actor = &session.authority_actor;
    let (sources, target) = adapter_route(&state, actor)?;
    let mut recipients = sources.clone();
    recipients.extend(sources.iter().filter_map(|source| {
        source
            .strip_prefix("external/discord/user/")
            .map(|id| format!("person/discord-{id}"))
    }));
    let mut changed = state.event_notify.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(query.wait_ms.min(10_000));
    let mut after = query.after;
    loop {
        let store = state.store.clone();
        let selected_target = target.clone();
        let selected_recipients = recipients.clone();
        let (items, frontier, through) = blocking_store(move || {
            let through = store.index()?;
            if after > through {
                return Err(St3Error::new(
                    "adapter-resync-required",
                    "the local delivery frontier moved backwards; reload retained delivery state",
                )
                .into());
            }
            let (items, next) = store.adapter_delivery_page(
                &selected_target,
                &selected_recipients,
                after,
                through,
                100,
            )?;
            Ok((items, next.unwrap_or(through), through))
        })
        .await?;
        if !items.is_empty() || frontier < through || tokio::time::Instant::now() >= deadline {
            return Ok(Json(
                json!({"contract":"st3.adapter.delivery.v0", "items":items, "cursor":frontier, "more":frontier < through, "host":state.node}),
            ));
        }
        after = frontier;
        if tokio::time::timeout_at(deadline, changed.changed())
            .await
            .is_err()
        {
            return Ok(Json(
                json!({"contract":"st3.adapter.delivery.v0", "items":[], "cursor":through, "more":false, "host":state.node}),
            ));
        }
    }
}

/// Import as an external sender while retaining the adapter's own upload authority.
/// The selected program-seat declaration binds an explicit account set to one destination.
pub(super) async fn import_message(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Json(mut request): Json<MessageSendRequest>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "control.messages")?;
    let actor = &session.authority_actor;
    let source = &request.from;
    if !actor.starts_with("agent/") || !external_source(source) || !request.to.starts_with("agent/")
    {
        return Err(ApiError::bad(St3Error::new(
            "adapter-route-refused",
            "an adapter import needs its agent actor and an external provider account",
        )));
    }
    let (sources, target) = adapter_route(&state, actor)?;
    if !sources.contains(source) || target != request.to {
        return Err(ApiError::bad(St3Error::new(
            "adapter-route-refused",
            "the program-seat declaration does not authorize this exact external sender and destination",
        )));
    }
    request.tags.push(format!("adapter:{actor}"));
    let receipt =
        accept_message_receipt_with_upload_owner(&state, request, None, None, Some(actor))?;
    Ok(Json(json!(receipt.message)))
}
