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
        store.observe_harness_control(&NativeState { subject: request.fence.subject.clone(), binding: binding.clone(), idle: observation.idle, input_supported: observation.input_supported, steer: observation.steer, models: observation.models, approval: observation.approval, pending_ask: observation.pending_ask, ask_supported: observation.ask_supported, ask_reason: observation.ask_reason, reason: observation.reason }, &request.fence)?;
        Ok(binding)
    }).await?;
    signal_local_change(&state);
    Ok(Json(binding))
}
pub(super) async fn next(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(fence): Json<crate::mailbox::Fence>) -> Result<Json<Option<ControlCommand>>, ApiError> {
    authorize(&fence, peer.as_ref().map(|peer| &peer.0))?;
    let store = state.store.clone();
    let command = blocking_action(move || {
        if let Some(command) = store.take_harness_ask(&fence)? { return Ok(Some(ControlCommand::AnswerAsk(command))); }
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
        } else if store.harness_ask_receipt(&request.receipt.operation_id)?.is_some() {
            serde_json::to_value(store.settle_harness_ask(&request.receipt, &request.fence)?).map_err(|error| St3Error::new("internal", error.to_string()))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AskTerminalRequest {
    fence: crate::mailbox::Fence,
    input: st3_schema::harness_control::AskTerminalInput,
}
pub(super) async fn ask_terminal_input(State(state): State<AppState>, peer: Option<Extension<NativeDeliveryPeer>>, Json(request): Json<AskTerminalRequest>) -> Result<Json<Value>, ApiError> {
    use st3_schema::harness_control::AskSurface;
    authorize(&request.fence, peer.as_ref().map(|peer| &peer.0))?;
    let subject = request.fence.subject.clone();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let native = state.store.harness_control_state(&subject).map_err(ApiError::bad)?
            .ok_or_else(|| ApiError::bad(St3Error::new("stale-harness-ask", "native state is unavailable")))?;
        if native.binding != request.input.binding {
            return Err(ApiError::bad(St3Error::new("stale-harness-ask", "native ask binding changed")));
        }
        let ask = native.pending_ask.filter(|ask| ask.tool_call_id == request.input.tool_call_id)
            .ok_or_else(|| ApiError::bad(St3Error::new("stale-harness-ask", "native ask ended")))?;
        let question = ask.questions.get(request.input.question_index)
            .ok_or_else(|| ApiError::bad(St3Error::new("invalid-harness-token", "terminal question index is invalid")))?;
        let session = live_session(&state, &subject, Some(&request.input.binding.incarnation_id))?;
        if !session.terminal || session.driver.as_deref() != Some("omp") {
            return Err(ApiError::bad(St3Error::new("unsupported-harness-ask", "native guarded ask input requires the OMP TUI")));
        }
        let screen = daemon_pty(&state).and_then(|runtime| runtime.screen(&session.runtime_id)).map_err(ApiError::internal)?;
        if screen.contains("Finish or clear the current prompt to answer") {
            return Err(ApiError::bad(St3Error::new("unsupported-harness-ask", "the native ask is blocked by an existing editor draft")));
        }
        // Conservative presentation fence: wrapped/ambiguous dialogs hand off to the terminal.
        // The native public input guard additionally checks every token at consumption time.
        let presented = match request.input.surface {
            AskSurface::Question => screen.contains(&question.question) && screen.contains("Other (type your own)") && (screen.contains("select") || screen.contains("toggle")),
            AskSurface::Custom => screen.contains("Custom answer") && screen.contains(&question.question),
            AskSurface::Review => screen.contains("Review answers") && screen.contains("Submit"),
        };
        if presented {
            let data = format!("\u{1b}[200~{}\u{1b}[201~", request.input.token);
            let store = state.store.clone();
            let runtime = daemon_pty(&state).map_err(ApiError::internal)?;
            // bind_mailbox takes this same writer lock: a same-incarnation lease
            // takeover cannot cross the authenticated reservation and actual write.
            blocking_action(move || store.send_harness_ask_token(&request.input, &request.fence, move || runtime.send_raw_if(&session.runtime_id, data.as_bytes(), Some(&session.incarnation_id)))).await?.map_err(ApiError::internal)?;
            return Ok(Json(json!({"transport":"written","answered":false})));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ApiError::bad(St3Error::new("unsupported-harness-ask", "the exact native ask input surface was not observed")));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
