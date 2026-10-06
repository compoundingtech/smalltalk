use super::*;
use crate::harness_events::{CurrentPublication, Publication};

pub(super) async fn publish(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Publication>,
) -> Result<Json<ClaimRecord>, ApiError> {
    guard_peer(peer, &request.claim)?;
    let store = state.store.clone();
    let kind = request.claim.kind.clone();
    let (record, changed) = blocking_action(move || store.append_harness_event(&request)).await?;
    finish_claim_publication(&state, &kind, record, changed).await
}

pub(super) async fn publish_current(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<CurrentPublication>,
) -> Result<Json<ClaimRecord>, ApiError> {
    guard_peer(peer, &request.claim)?;
    let store = state.store.clone();
    let (record, changed) = blocking_action(move || store.append_harness_current(&request)).await?;
    finish_claim_publication(&state, "harness.current", record, changed).await
}

fn guard_peer(peer: Option<Extension<NativeDeliveryPeer>>, claim: &ClaimInput) -> Result<(), ApiError> {
    let Some(peer) = peer else {
        return Err(ApiError::bad(St3Error::new(
            "unbound-harness-event",
            "harness observations require a local native driver",
        )));
    };
    if peer.agent != claim.subject
        || claim.actor.as_deref() != Some(&peer.agent)
        || !peer.archives_inbox && peer.transport != "pi-channel" && peer.transport != "omp-channel"
    {
        return Err(ApiError::bad(St3Error::new(
            "foreign-harness-event",
            "only this seat's native driver can publish its observations",
        )));
    }
    Ok(())
}
