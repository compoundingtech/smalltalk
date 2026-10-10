//! Prototype-only revision-fenced, full-roster agents long poll.
use super::*;
use crate::store::AgentsStatusWatermark;

#[derive(Default, Deserialize)]
pub(super) struct PollQuery {
    node_epoch: Option<String>,
    after_revision: Option<u64>,
    min_store_index: Option<u64>,
    min_local_frontier: Option<u64>,
    wait_ms: Option<u64>,
}

pub(super) async fn poll(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<PollQuery>,
) -> Result<(Extension<ClientSnapshot>, Json<Value>), ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    if !state.store.agent_roster_refresher_running() {
        return Err(agent_roster_not_ready());
    }
    // Subscribe first, then capture both status frontiers. No request reader, admission,
    // SQLite transaction or WAL read mark survives either await below.
    let mut changed = state.store.subscribe_agent_roster();
    let reader = state.clone();
    let captured = blocking_store(move || reader.store.agent_status_watermark()).await?;
    let required = AgentsStatusWatermark {
        store_index: query.min_store_index.unwrap_or(captured.store_index),
        local_frontier: query.min_local_frontier.unwrap_or(captured.local_frontier),
    };
    let epoch = state.store.agent_roster_epoch().to_owned();
    let after = query.after_revision.unwrap_or(0);
    if query.node_epoch.as_deref().is_some_and(|previous| previous != epoch) {
        return Ok((Extension(new_client_snapshot(&state)), Json(json!({
            "kind":"resync", "node_epoch":epoch, "reason":"node-epoch-changed"
        }))));
    }
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(query.wait_ms.unwrap_or(25_000).min(30_000));
    state.store.request_fresh_agent_roster(false);
    loop {
        if let Some((publication, cards)) = state.store.agents_publication(u64::MAX) {
            if publication.revision > after && publication.status_watermark.covers(required) {
                let reader = state.clone();
                let session = session.clone();
                return blocking_store(move || {
                    let current = if session.transport == "unix" {
                        session
                    } else {
                        match client_v0::revalidate_session(&reader, &session) {
                            Ok(current) => current,
                            Err(error) => return Ok(Err(error)),
                        }
                    };
                    if let Err(error) = client_v0::require_scope(&current, "read.projections") {
                        return Ok(Err(error));
                    }
                    let mut snapshot = roster_snapshot(&reader,
                        publication.status_watermark.store_index, publication.materialized_at_ms);
                    snapshot.publication = Some(publication.clone());
                    Ok(Ok((Extension(snapshot), Json(json!({
                        "kind":"snapshot", "publication":publication, "items":cards.as_ref()
                    })))))
                }).await?;
            }
        }
        match tokio::time::timeout_at(deadline, changed.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(agent_roster_not_ready()),
            Err(_) => {
                // Explicit non-snapshot result: never masquerade stale rows as success.
                return Ok((Extension(new_client_snapshot(&state)), Json(json!({
                    "kind":"unchanged", "reason":"timeout", "node_epoch":epoch,
                    "after_revision":after, "required_status_watermark":required
                }))));
            }
        }
    }
}
