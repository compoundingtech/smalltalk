//! Daemon-owned approval asks and transaction-bound assistant write provenance.
use super::*;

pub async fn propose(State(state): State<AppState>, Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<ClientSession>, Json(request): Json<ActionRequest>) -> Result<Json<Value>, ApiError> {
    if !matches!(request.action_type.as_str(), "work.done" | "message.send" | "custom.reply") {
        return Err(forbidden("only typed person answers, custom replies and message sends may be proposed"));
    }
    let required_scope = match request.action_type.as_str() {
        "message.send" => "write.assistant.messages", "custom.reply" => "write.assistant.custom-reply",
        _ => "read.assistant.attention",
    };
    if !session.allows(required_scope) {
        return Err(forbidden("proposal scope is absent"));
    }
    let grant_subject = session.pairing_grant.as_deref().ok_or_else(|| forbidden("assistant grant absent"))?;
    let _guard = TRANSITIONS.lock().await;
    let granted = latest(&state,grant_subject,GRANTED)?.ok_or_else(|| forbidden("assistant grant absent"))?;
    let current_session = super::session(&state,&granted,session.transport)?;
    if !current_session.allows(required_scope) {
        return Err(forbidden("proposal scope is absent"));
    }
    let owner = owning_person(&state)?.to_owned();
    let policy = effective(&state,&granted)?;
    let authorization = crate::store::assistant_write::AssistantWriteAuthorization {
        grant_subject: grant_subject.into(), actor: session.actor.clone(), owner: owner.clone(),
        policy_revision: fields(&policy)?["policy_revision"].as_u64().ok_or_else(|| forbidden("assistant policy revision absent"))?,
        scope: required_scope.into(),
        recipient: if request.action_type == "message.send" { Some(parameter_string(&request.parameters,"to")?) } else { None },
        approval_subject: None,
    };
    if matches!(request.action_type.as_str(), "message.send" | "custom.reply") {
        if request.parameters.get("signature").is_some() {
            return Err(forbidden("assistant writes cannot supply person signatures"));
        }
        if request.action_type == "message.send" {
            if ["from", "actor", "sender_class", "assistant_grant_id", "assistant_owner", "assistant_session_actor"].iter().any(|field| request.parameters.get(*field).is_some()) {
                return Err(forbidden("assistant message identity and provenance are server-owned"));
            }
            let to = parameter_string(&request.parameters,"to")?;
            if !fields(&policy)?["recipient_allowlist"].as_array().is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&to))) {
                return Err(forbidden("message recipient is not owner-allowlisted"));
            }
        }
        let mut owner_session = ClientSession::local(Some(&owner))?;
        // The owner supplies delegated authority only. The message action selects PRINCIPAL
        // as its sender and the writer records owner/session/grant provenance atomically.
        owner_session.actor = session.actor.clone();
        owner_session.pairing_grant = current_session.pairing_grant.clone();
        owner_session.grant_expires_at = current_session.grant_expires_at;
        owner_session.assistant_write = Some(authorization);
        drop(_guard);
        let result = super::super::action(State(state.clone()),Extension(snapshot),Extension(owner_session),Json(request)).await;
        audit(&state,"grant-authorized-write",&session.actor,session.transport,fields(&granted)?["grant_id"].as_str(),&result)?;
        return result;
    }
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&json!({"grant":grant_subject,"request":request})).map_err(ApiError::internal)?));
    let subject = format!("step-run/chatgpt-{digest}/ask");
    state.store.ask_daemon_approval(&owner,&digest,serde_json::to_value(&request).map_err(ApiError::internal)?)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    let ask = state.store.step_run(&subject).map_err(ApiError::internal)?.ok_or_else(|| ApiError::internal("approval ask absent"))?;
    let approved = ask.person_answers.iter().any(|answer| answer.respondent == owner && answer.acted_for.is_none()
        && answer.status == "completed" && answer.answer.as_ref().is_some_and(|value| value["id"] == "approve"));
    if !approved {
        return Ok(Json(json!({"status":if ask.person_answers.is_empty() {"approval-required"} else {"approval-denied"},"approval_id":subject,"request_digest":digest})));
    }
    // Execute via the same fenced, idempotent typed action path as the owner's CLI/UI.
    let mut owner_session = ClientSession::local(Some(&owner))?;
    owner_session.actor = session.actor.clone();
    owner_session.pairing_grant = current_session.pairing_grant.clone();
    owner_session.grant_expires_at = current_session.grant_expires_at;
    owner_session.assistant_write = Some(crate::store::assistant_write::AssistantWriteAuthorization {
        approval_subject: Some(subject), ..authorization
    });
    drop(_guard);
    let result = super::super::action(State(state.clone()),Extension(snapshot),Extension(owner_session),Json(request)).await;
    audit(&state,"approved-write",&session.actor,session.transport,fields(&granted)?["grant_id"].as_str(),&result)?;
    result
}
