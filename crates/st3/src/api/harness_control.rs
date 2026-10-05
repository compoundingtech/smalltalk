//! Same-seat native controls. The local owner alone reserves delivery and validates settlement.
use super::*;
use st3_schema::harness_control::{Binding, ControlCommand, NativeObservation, NativeReceipt, NativeState};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ObservationRequest {
    fence: crate::mailbox::Fence,
    observation: NativeObservation,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptRequest {
    fence: crate::mailbox::Fence,
    receipt: NativeReceipt,
}
fn authorize(fence: &crate::mailbox::Fence, peer: Option<&NativeDeliveryPeer>) -> Result<(), ApiError> {
    if fence.component != "delivery" || peer.is_none_or(|peer| peer.agent != fence.subject || peer.transport != "omp-channel") {
        return Err(ApiError::bad(St3Error::new("foreign-harness-control", "native control requires the same OMP delivery driver")));
    }
    Ok(())
}
pub(super) async fn observe(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(request): Json<ObservationRequest>) -> Result<Json<Binding>, ApiError> {
    authorize(&request.fence, peer.as_ref().map(|peer| &peer.0))?;
    let store = state.store.clone();
    let binding = blocking_action(move || {
        let observation = request.observation;
        let binding = Binding { desired_revision: store.harness_control_desired_revision(&request.fence)?, incarnation_id: request.fence.incarnation.clone(), session_id: observation.session_id, turn_id: observation.turn_id };
        store.observe_harness_control(&NativeState { subject: request.fence.subject.clone(), binding: binding.clone(), idle: observation.idle, input_supported: observation.input_supported, steer: observation.steer, models: observation.models, approval: observation.approval, reason: observation.reason }, &request.fence)?;
        Ok(binding)
    }).await?;
    signal_local_change(&state);
    Ok(Json(binding))
}
pub(super) async fn next(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(fence): Json<crate::mailbox::Fence>) -> Result<Json<Option<ControlCommand>>, ApiError> {
    authorize(&fence, peer.as_ref().map(|peer| &peer.0))?;
    let store = state.store.clone();
    let command = blocking_action(move || {
        if let Some(command) = store.take_harness_model(&fence)? { return Ok(Some(ControlCommand::SetModel(command))); }
        store.take_harness_input(&fence.subject, &fence).map(|command| command.map(ControlCommand::Input))
    }).await?;
    if command.is_some() { signal_local_change(&state); }
    Ok(Json(command))
}
pub(super) async fn settle(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(request): Json<ReceiptRequest>) -> Result<Json<Value>, ApiError> {
    authorize(&request.fence, peer.as_ref().map(|peer| &peer.0))?;
    let store = state.store.clone();
    let receipt = blocking_action(move || {
        if store.harness_model_receipt(&request.receipt.operation_id)?.is_some() {
            serde_json::to_value(store.settle_harness_model(&request.receipt, &request.fence)?).map_err(|error| St3Error::new("internal", error.to_string()))
        } else {
            serde_json::to_value(store.settle_harness_input(&request.receipt, &request.fence)?).map_err(|error| St3Error::new("internal", error.to_string()))
        }
    }).await?;
    signal_local_change(&state);
    Ok(Json(receipt))
}
pub(super) async fn close(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(fence): Json<crate::mailbox::Fence>) -> Result<Json<Value>, ApiError> {
    authorize(&fence, peer.as_ref().map(|peer| &peer.0))?;
    let store = state.store.clone();
    blocking_action(move || store.close_harness_control(&fence)).await?;
    signal_local_change(&state);
    Ok(Json(json!({"closed":true})))
}
