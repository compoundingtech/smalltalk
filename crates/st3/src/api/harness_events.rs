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
    let notes_incarnation = request.runtime_incarnation.clone();
    let notes_agent = peer.agent.clone();
    let (record, changed) = blocking_action(move || store.append_harness_event(&request)).await?;
    if let Err(error) = client_v0::private_notes::bind_native(&state, &notes_agent, &notes_incarnation).await {
        // Optional notes admission never stalls the already committed producer outbox.
        // Reads reprove realization and source incarnation; no notes are supplied on failure.
        tracing::warn!(subject = %notes_agent, code = %error.code, "private notes source unavailable");
    }
    finish_claim_publication(&state, &kind, record, changed).await
}
