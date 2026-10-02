use super::*;
use crate::harness_events::Publication;

pub(super) async fn publish(
    State(state): State<AppState>,
    peer: Option<Extension<NativeDeliveryPeer>>,
    Json(request): Json<Publication>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let Some(peer) = peer else {
        return Err(ApiError::bad(St3Error::new(
            "unbound-harness-event",
            "harness events require a local native driver",
        )));
    };
    if peer.agent != request.claim.subject
        || request.claim.actor.as_deref() != Some(&peer.agent)
        || !peer.archives_inbox && peer.transport != "pi-channel" && peer.transport != "omp-channel"
    {
        return Err(ApiError::bad(St3Error::new(
            "foreign-harness-event",
            "only this seat's native driver can publish its observations",
        )));
    }
    let store = state.store.clone();
    let kind = request.claim.kind.clone();
    let (record, changed) = blocking_action(move || store.append_harness_event(&request)).await?;
    finish_claim_publication(&state, &kind, record, changed).await
}
