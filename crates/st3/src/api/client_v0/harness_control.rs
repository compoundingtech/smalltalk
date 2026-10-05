//! Paired controls route to the runtime owner, never to a gateway's empty local queue.
use super::*;
use st3_schema::harness_control::{ControlOperationReceipt, ControlState, ModelsSummary, QueueParameters, QueueRequest};

pub(super) fn owner(state: &AppState, subject: &str) -> Result<String, ApiError> {
    let status = state.store.status(Some(subject)).map_err(ApiError::internal)?;
    let subject = status.subjects.first().ok_or_else(|| ApiError::not_found("harness does not exist"))?;
    let origin = subject.actual_origin.as_deref().ok_or_else(|| ApiError::not_found("harness has no observed owner"))?;
    Ok(client_host_id(origin))
}
pub(super) async fn relay(state: &AppState, session: &ClientSession, owner: &str, request: crate::peer::ClientReadOperation) -> Result<Value, ApiError> {
    state.client_relay.as_ref().ok_or_else(|| remote_unavailable(owner))?.read(owner, &crate::peer::ClientReadRequest { authority_actor: session.authority_actor.clone(), relay: None, request }).await.map_err(|error| remote_read_error(owner, error))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct QueueQuery { cursor: Option<String>, limit: Option<usize> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QueueCursor { subject: String, revision: u64, offset: usize }
fn encode_queue_cursor(state: &AppState, cursor: &QueueCursor) -> Result<String, ApiError> {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).map_err(ApiError::internal)?);
    let signature = derive_terminal_capability(state, "harness-queue-cursor.v1", &payload, &client_host_id(&state.node))?;
    Ok(format!("harness-queue/{payload}.{signature}"))
}
fn decode_queue_cursor(state: &AppState, encoded: &str) -> Result<QueueCursor, ApiError> {
    if encoded.len() > 16384 { return Err(validation("queue cursor exceeds its bound")); }
    let (payload, signature) = encoded.strip_prefix("harness-queue/").and_then(|encoded| encoded.split_once('.')).ok_or_else(|| validation("invalid queue cursor"))?;
    let expected = derive_terminal_capability(state, "harness-queue-cursor.v1", payload, &client_host_id(&state.node))?;
    if expected.len() != signature.len() || expected.bytes().zip(signature.bytes()).fold(0_u8, |difference, (left, right)| difference | (left ^ right)) != 0 { return Err(validation("invalid queue cursor signature")); }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).map_err(|_| validation("invalid queue cursor"))?;
    serde_json::from_slice(&bytes).map_err(|_| validation("invalid queue cursor"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ReceiptQuery { subject: String }
struct ByteCount(usize);
impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0 += bytes.len(); Ok(bytes.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
pub(in crate::api) async fn queue(State(state): State<AppState>, Extension(session): Extension<ClientSession>, AxumPath(subject): AxumPath<String>, Query(query): Query<QueueQuery>) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let subject = if subject.starts_with("agent/") { subject } else { format!("agent/{subject}") };
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) { return Err(validation("queue page limit must be 1 to 100")); }
    let host = owner(&state, &subject)?;
    if host != client_host_id(&state.node) {
        return Ok(Json(relay(&state, &session, &host, crate::peer::ClientReadOperation::HarnessQueue { subject, cursor: query.cursor, limit: query.limit }).await?));
    }
    let mut queue = state.store.harness_control_queue(&subject).map_err(ApiError::bad)?;
    let native = state.store.harness_control_state(&subject).map_err(ApiError::bad)?.map(|native| ControlState { subject: native.subject, binding: native.binding, idle: native.idle, input_supported: native.input_supported, steer: native.steer, models: ModelsSummary { selected: native.models.selected, revision: native.models.revision, available: native.models.available, complete: native.models.complete, atomic_model_effort: native.models.atomic_model_effort, source: native.models.source, catalog_path: format!("/v1/client/harness-models/{}", urlencoding::encode(&subject)) }, approval: native.approval, reason: native.reason });
    let total = queue.entries.len();
    let offset = if let Some(cursor) = query.cursor {
        let cursor = decode_queue_cursor(&state, &cursor)?;
        if cursor.subject != subject || cursor.revision != queue.revision { return Err(ApiError::bad(St3Error::new("stale-queue-cursor", "the owner queue changed; restart pagination"))); }
        if cursor.offset > total { return Err(validation("queue cursor is outside its revision")); }
        cursor.offset
    } else { 0 };
    let mut metadata = ByteCount(0);
    serde_json::to_writer(&mut metadata, &native).map_err(ApiError::internal)?;
    let mut count = 0; let mut bytes = 4096 + subject.len() * 6 + metadata.0;
    if bytes > 512 * 1024 { return Err(ApiError::bad(St3Error::new("queue-metadata-too-large", "native queue metadata exceeds the page transport bound"))); }
    for entry in queue.entries.iter().skip(offset).take(limit) {
        let mut counter = ByteCount(0);
        serde_json::to_writer(&mut counter, entry).map_err(ApiError::internal)?;
        if bytes + counter.0 > 512 * 1024 { break; }
        bytes += counter.0; count += 1;
    }
    if count == 0 && offset < total { return Err(ApiError::bad(St3Error::new("queue-entry-too-large", "one queue entry exceeds the page transport bound"))); }
    let next = offset + count;
    let cursor = if next < total { Some(encode_queue_cursor(&state, &QueueCursor { subject: subject.clone(), revision: queue.revision, offset: next })?) } else { None };
    queue.entries.drain(..offset); queue.entries.truncate(count);
    Ok(Json(json!({"schema":"harness-queue.v1", "subject":subject,"queue":queue,"native":native,"cursor":cursor,"total":total})))
}
pub(in crate::api) async fn receipt(State(state): State<AppState>, Extension(session): Extension<ClientSession>, AxumPath(operation): AxumPath<String>, Query(query): Query<ReceiptQuery>) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let host = owner(&state, &query.subject)?;
    if host != client_host_id(&state.node) { return Ok(Json(relay(&state, &session, &host, crate::peer::ClientReadOperation::HarnessControlReceipt { subject: query.subject, operation }).await?)); }
    let receipt = if let Some(receipt) = state.store.harness_control_receipt(&operation).map_err(ApiError::bad)?.filter(|receipt| receipt.subject == query.subject) {
        ControlOperationReceipt::Queue(receipt)
    } else if let Some(receipt) = state.store.harness_model_receipt(&operation).map_err(ApiError::bad)?.filter(|receipt| receipt.subject == query.subject) {
        ControlOperationReceipt::Model(receipt)
    } else {
        return Err(ApiError::not_found("harness operation does not exist on this owner"));
    };
    Ok(Json(serde_json::to_value(receipt).map_err(ApiError::internal)?))
}
pub(super) async fn mutate(state: &AppState, session: &ClientSession, request: &ActionRequest) -> Result<Value, ApiError> {
    if !session.authority_actor.starts_with("person/") || session.authority_actor.matches('/').count() != 1 || session.authority_actor == "person/" { return Err(forbidden("harness control requires a concrete person")); }
    let parameters: QueueParameters = serde_json::from_value(request.parameters.clone()).map_err(|error| validation(error.to_string()))?;
    if !parameters.subject.starts_with("agent/") { return Err(validation("harness target must be an agent subject")); }
    if request.fence.runtime_incarnation.as_deref() != Some(parameters.binding.incarnation_id.as_str()) || request.fence.runtime_desired_revision.as_deref() != Some(parameters.binding.desired_revision.as_str()) { return Err(validation("native binding must match both runtime fences")); }
    let host = owner(state, &parameters.subject)?;
    if host != client_host_id(&state.node) {
        let mut value = relay(state, session, &host, crate::peer::ClientReadOperation::HarnessQueueMutation { action_id: request.id.clone(), idempotency_key: request.idempotency_key.clone(), parameters: request.parameters.clone() }).await?;
        value["snapshot_id"] = json!(new_client_snapshot(state).id);
        return Ok(value);
    }
    let receipt = state.store.mutate_harness_queue(&QueueRequest { subject: parameters.subject, actor: session.authority_actor.clone(), idempotency_key: request.idempotency_key.clone(), binding: parameters.binding, queue_revision: parameters.queue_revision, mutation: parameters.mutation }).map_err(ApiError::bad)?;
    signal_local_change(state);
    Ok(json!({"kind":"action-result", "action_id":request.id, "operation_id":receipt.operation_id,"status":receipt.status,"affected_ids":receipt.entry_id.iter().collect::<Vec<_>>(),"harness_control":receipt,"snapshot_id":new_client_snapshot(state).id}))
}
