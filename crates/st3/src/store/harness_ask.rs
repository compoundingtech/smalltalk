//! Atomic native ask answers use the existing owner dispatch interlock, never the chat queue.
use super::*;
use super::harness_control::{check_binding, check_runtime, release_dispatch_tx, reserve_dispatch_tx, state_tx};
use st3_schema::harness_control::{AskCommand, AskIndeterminateReason, AskOutcome, AskParameters, AskReceipt, AskRequest, AskTerminalInput, NativeReceipt, NativeResult, Outcome};

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS local_harness_ask_operations(id TEXT PRIMARY KEY,subject TEXT NOT NULL,digest TEXT NOT NULL,parameters TEXT NOT NULL,receipt TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS local_harness_ask_subject ON local_harness_ask_operations(subject);
        CREATE TABLE IF NOT EXISTS local_harness_ask_tokens(token TEXT PRIMARY KEY,operation_id TEXT NOT NULL);")?;
    Ok(())
}
fn receipt_tx(tx: &Connection, operation: &str) -> Result<Option<AskReceipt>, St3Error> {
    let stored: Option<String> = tx.query_row("SELECT receipt FROM local_harness_ask_operations WHERE id=?1", [operation], |row| row.get(0)).optional().map_err(internal)?;
    stored.map(|value| serde_json::from_str(&value).map_err(internal)).transpose()
}
fn save(tx: &Connection, receipt: &AskReceipt) -> Result<(), St3Error> {
    tx.execute("UPDATE local_harness_ask_operations SET receipt=?2 WHERE id=?1 AND json_extract(receipt,'$.status') IN ('accepted','dispatched')", params![receipt.operation_id, serde_json::to_string(receipt).map_err(internal)?]).map_err(internal)?;
    Ok(())
}
pub(super) fn invalidate_binding_tx(tx: &Connection, subject: &str, reason: &str) -> Result<(), St3Error> {
    let mut statement = tx.prepare("SELECT receipt FROM local_harness_ask_operations WHERE subject=?1 AND json_extract(receipt,'$.status') IN ('accepted','dispatched')").map_err(internal)?;
    let stored = statement.query_map([subject], |row| row.get::<_, String>(0)).map_err(internal)?.collect::<rusqlite::Result<Vec<_>>>().map_err(internal)?;
    drop(statement);
    for value in stored {
        let mut receipt: AskReceipt = serde_json::from_str(&value).map_err(internal)?;
        receipt.status = if receipt.status == Outcome::Dispatched { Outcome::Indeterminate } else { Outcome::Rejected };
        receipt.reason = Some(reason.into());
        save(tx, &receipt)?;
        release_dispatch_tx(tx, subject, &receipt.operation_id)?;
    }
    Ok(())
}
fn validate(tx: &Connection, parameters: &AskParameters) -> Result<bool, St3Error> {
    let state = check_binding(tx, &parameters.subject, &parameters.binding)?;
    let ask = state.pending_ask.ok_or_else(|| St3Error::new("already-settled", "there is no live native ask"))?;
    if ask.tool_call_id != parameters.tool_call_id { return Err(St3Error::new("stale-harness-ask", "the native ask changed")); }
    let conflicted = state.ask_reason.as_deref() == Some("terminal-input-conflict");
    if !state.ask_supported && !conflicted { return Err(St3Error::new("unsupported-harness-ask", state.ask_reason.unwrap_or_else(|| "native ask answer route is unavailable".into()))); }
    if parameters.answers.len() != ask.questions.len() { return Err(St3Error::new("invalid-harness-answers", "all questions must be answered together in original order")); }
    for (question, answer) in ask.questions.iter().zip(&parameters.answers) {
        let custom = answer.custom_input.as_deref();
        if answer.id != question.id || answer.selected_options.iter().any(|label| !question.options.iter().any(|option| &option.label == label))
            || answer.selected_options.iter().enumerate().any(|(index, label)| answer.selected_options[..index].contains(label))
            || custom.is_some_and(|value| value.trim().is_empty() || value.len() > 64 * 1024 || value.chars().any(|ch| ch.is_control() && ch != '\n' && ch != '\t'))
            || (!question.multi.unwrap_or(false) && answer.selected_options.len() + usize::from(custom.is_some()) != 1) {
            return Err(St3Error::new("invalid-harness-answers", "answers must match question IDs, choices, cardinality, and safe nonempty custom text"));
        }
    }
    Ok(conflicted)
}
impl Store {
    /// Replays recover original answers and return frozen proof, never another terminal write.
    pub fn harness_ask_replay(&self, actor: &str, key: &str) -> Result<Option<(AskParameters, AskReceipt)>, St3Error> {
        let operation = format!("operation/harness-ask-{}", hex::encode(Sha256::digest(serde_json::to_vec(&(actor, key)).map_err(internal)?)));
        let connection = self.readers.get();
        let stored: Option<(String, String)> = connection.query_row("SELECT parameters,receipt FROM local_harness_ask_operations WHERE id=?1", [&operation], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(internal)?;
        stored.map(|(parameters, receipt)| Ok((serde_json::from_str(&parameters).map_err(internal)?, serde_json::from_str(&receipt).map_err(internal)?))).transpose()
    }
    pub fn reserve_harness_ask(&self, request: &AskRequest) -> Result<AskReceipt, St3Error> {
        if !request.actor.starts_with("person/") || request.actor.matches('/').count() != 1 || request.actor == "person/" { return Err(St3Error::new("forbidden", "native ask answers require a concrete person")); }
        if !(16..=256).contains(&request.idempotency_key.len()) { return Err(St3Error::new("invalid-idempotency-key", "idempotency key must contain 16 to 256 bytes")); }
        let p = &request.parameters;
        let operation = format!("operation/harness-ask-{}", hex::encode(Sha256::digest(serde_json::to_vec(&(&request.actor, &request.idempotency_key)).map_err(internal)?)));
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&(&request.actor, p)).map_err(internal)?));
        self.connection.batched(|tx| -> Result<AskReceipt, St3Error> {
            let prior: Option<(String, String)> = tx.query_row("SELECT digest,receipt FROM local_harness_ask_operations WHERE id=?1", [&operation], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(internal)?;
            if let Some((stored, value)) = prior {
                if stored != digest { return Err(St3Error::new("idempotency-conflict", "this operation key identifies different answers")); }
                return serde_json::from_str(&value).map_err(internal);
            }
            let conflicted = validate(tx, p)?;
            let settled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM local_harness_ask_operations WHERE subject=?1 AND json_extract(parameters,'$.tool_call_id')=?2 AND json_extract(parameters,'$.binding')=json(?3) AND json_extract(receipt,'$.status') IN ('accepted','dispatched','applied','indeterminate'))", params![p.subject, p.tool_call_id, serde_json::to_string(&p.binding).map_err(internal)?], |row| row.get(0)).map_err(internal)?;
            if settled { return Err(St3Error::new("already-settled", "this ask already has an admitted answer")); }
            // Native answer input shares the native mutation lane but is not a queued chat entry.
            if !conflicted && !reserve_dispatch_tx(tx, &p.subject, &operation, "input", &p.binding)? { return Err(St3Error::new("harness-control-busy", "another native mutation owns the control lane")); }
            let receipt = AskReceipt {
                operation_id: operation.clone(), subject: p.subject.clone(), binding: p.binding.clone(), tool_call_id: p.tool_call_id.clone(),
                status: if conflicted { Outcome::Indeterminate } else { Outcome::Accepted },
                reason: conflicted.then(|| "terminal-input-conflict".into()), result: None,
                outcome: conflicted.then_some(AskOutcome::Indeterminate { reason: AskIndeterminateReason::TerminalInputConflict }),
            };
            tx.execute("INSERT INTO local_harness_ask_operations(id,subject,digest,parameters,receipt) VALUES(?1,?2,?3,?4,?5)", params![operation, p.subject, digest, serde_json::to_string(p).map_err(internal)?, serde_json::to_string(&receipt).map_err(internal)?]).map_err(internal)?;
            Ok(receipt)
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn take_harness_ask(&self, fence: &crate::mailbox::Fence) -> Result<Option<AskCommand>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<AskCommand>, St3Error> {
            check_mailbox_fence(tx, fence)?;
            let operation: Option<String> = tx.query_row("SELECT a.id FROM local_harness_ask_operations a JOIN local_harness_control_dispatch d ON d.operation_id=a.id AND d.subject=a.subject WHERE a.subject=?1 AND json_extract(a.receipt,'$.status')='accepted'", [&fence.subject], |row| row.get(0)).optional().map_err(internal)?;
            let Some(operation) = operation else { return Ok(None); };
            let mut receipt = receipt_tx(tx, &operation)?.ok_or_else(|| St3Error::new("missing-harness-operation", "ask operation disappeared"))?;
            let stored: String = tx.query_row("SELECT parameters FROM local_harness_ask_operations WHERE id=?1", [&operation], |row| row.get(0)).map_err(internal)?;
            let p: AskParameters = serde_json::from_str(&stored).map_err(internal)?;
            match validate(tx, &p) {
                Ok(false) => {}
                Ok(true) => {
                    receipt.status = Outcome::Indeterminate; receipt.reason = Some("terminal-input-conflict".into());
                    receipt.outcome = Some(AskOutcome::Indeterminate { reason: AskIndeterminateReason::TerminalInputConflict });
                    save(tx, &receipt)?; release_dispatch_tx(tx, &fence.subject, &operation)?; return Ok(None);
                }
                Err(error) => {
                    receipt.status = Outcome::Rejected; receipt.reason = Some(error.code.into()); save(tx, &receipt)?; release_dispatch_tx(tx, &fence.subject, &operation)?; return Ok(None);
                }
            }
            receipt.status = Outcome::Dispatched; save(tx, &receipt)?;
            Ok(Some(AskCommand { operation_id: operation, binding: p.binding, tool_call_id: p.tool_call_id, answers: p.answers }))
        }).map_err(|error| St3Error::new("internal", error))?
    }
    /// Hold the mailbox writer fence through the PTY handoff. An uncertain transport
    /// error is an inner result so its one-use token still commits and cannot replay.
    /// Transport acknowledgements never settle the actual native answer.
    pub fn send_harness_ask_token(&self, input: &AskTerminalInput, fence: &crate::mailbox::Fence, send: impl FnOnce() -> anyhow::Result<()> + Send) -> Result<anyhow::Result<()>, St3Error> {
        self.connection.batched(|tx| -> Result<anyhow::Result<()>, St3Error> {
            check_mailbox_fence(tx, fence)?;
            let receipt = receipt_tx(tx, &input.operation_id)?.ok_or_else(|| St3Error::new("missing-harness-operation", "ask operation does not exist"))?;
            if receipt.subject != fence.subject || receipt.binding != input.binding || receipt.tool_call_id != input.tool_call_id || receipt.status != Outcome::Dispatched { return Err(St3Error::new("stale-harness-ask", "terminal token does not belong to the dispatched ask")); }
            let state = check_binding(tx, &receipt.subject, &receipt.binding)?;
            if !state.pending_ask.as_ref().is_some_and(|ask| ask.tool_call_id == input.tool_call_id) { return Err(St3Error::new("stale-harness-ask", "native ask already ended")); }
            if !(32..=128).contains(&input.token.len()) || !input.token.bytes().all(|ch| ch.is_ascii_alphanumeric() || ch == b'-') { return Err(St3Error::new("invalid-harness-token", "invalid native terminal input token")); }
            if tx.execute("INSERT OR IGNORE INTO local_harness_ask_tokens(token,operation_id) VALUES(?1,?2)", params![input.token, input.operation_id]).map_err(internal)? != 1 { return Err(St3Error::new("already-dispatched", "terminal token is never replayed")); }
            Ok(send())
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn settle_harness_ask(&self, native: &NativeReceipt, fence: &crate::mailbox::Fence) -> Result<AskReceipt, St3Error> {
        if !matches!(native.status, Outcome::Applied | Outcome::Rejected | Outcome::Indeterminate) { return Err(St3Error::new("invalid-native-receipt", "native settlement must be terminal")); }
        let result = match &native.result { Some(NativeResult::Ask(result)) => Some(result), None if native.status != Outcome::Applied => None, _ => return Err(St3Error::new("invalid-native-receipt", "ask settlement requires native ask evidence")) };
        self.connection.batched(|tx| -> Result<AskReceipt, St3Error> {
            check_mailbox_fence(tx, fence)?;
            let mut receipt = receipt_tx(tx, &native.operation_id)?.ok_or_else(|| St3Error::new("missing-harness-operation", "ask operation does not exist"))?;
            if native.subject != fence.subject || native.subject != receipt.subject || native.binding != receipt.binding { return Err(St3Error::new("stale-harness-ask", "native result does not match the reserved ask binding")); }
            if receipt.status != Outcome::Dispatched {
                if receipt.status == native.status && receipt.reason == native.reason && receipt.result.as_ref() == result { return Ok(receipt); }
                return Err(St3Error::new("already-settled", "native ask operation is already settled"));
            }
            check_runtime(tx, &native.subject, &native.binding)?;
            let state = state_tx(tx, &native.subject)?.ok_or_else(|| St3Error::new("stale-harness-ask", "native state is unavailable"))?;
            if state.binding != native.binding { return Err(St3Error::new("stale-harness-ask", "native activity changed before settlement")); }
            if native.status == Outcome::Applied {
                let stored: String = tx.query_row("SELECT parameters FROM local_harness_ask_operations WHERE id=?1", [&native.operation_id], |row| row.get(0)).map_err(internal)?;
                let p: AskParameters = serde_json::from_str(&stored).map_err(internal)?;
                if !matches!(result, Some(result) if result.native_event == "tool_result" && result.tool_call_id == p.tool_call_id && result.answers == p.answers) { return Err(St3Error::new("invalid-native-receipt", "applied ask requires the exact matching actual native answers")); }
            }
            receipt.status = native.status; receipt.reason.clone_from(&native.reason); receipt.result = result.cloned();
            receipt.outcome = if native.status == Outcome::Indeterminate && native.reason.as_deref() == Some("terminal-input-conflict") {
                Some(AskOutcome::Indeterminate { reason: AskIndeterminateReason::TerminalInputConflict })
            } else { None };
            save(tx, &receipt)?; release_dispatch_tx(tx, &native.subject, &native.operation_id)?;
            Ok(receipt)
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn harness_ask_receipt(&self, operation: &str) -> Result<Option<AskReceipt>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<AskReceipt>, St3Error> {
            let Some(receipt) = receipt_tx(tx, operation)? else { return Ok(None); };
            if matches!(receipt.status, Outcome::Accepted | Outcome::Dispatched) {
                if let Err(error) = check_runtime(tx, &receipt.subject, &receipt.binding) {
                    if !matches!(error.code, "stale-harness-control" | "stale-mailbox-session") { return Err(error); }
                    invalidate_binding_tx(tx, &receipt.subject, "native-runtime-ended-or-replaced")?;
                }
            }
            receipt_tx(tx, operation)
        }).map_err(|error| St3Error::new("internal", error))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st3_schema::harness_control::{Approval, AskAnswer, AskOption, AskQuestion, AskResult, Binding, Models, NativeState, PendingAsk};
    const SUBJECT: &str = "agent/ask-control";
    fn baseline() -> (Store, NativeState, crate::mailbox::Fence) {
        let store = Store::open_memory("ask-owner").unwrap();
        store.connection.batched(|tx| tx.execute("INSERT INTO desired(subject,kind,revision,claim_id,body) VALUES(?1,'agent','revision','desired-1','{}')", [SUBJECT])).unwrap().unwrap();
        store.append_claim(&ClaimInput { subject: SUBJECT.into(), kind: "runtime.observed".into(), actor: Some(SUBJECT.into()), fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!("incarnation-1")), ("runtime_id".into(), json!("native-runtime"))]), evidence: Vec::new(), expected_subject: None, idempotency_key: None }).unwrap();
        let fence = store.bind_mailbox(&crate::mailbox::Fence::new(SUBJECT, "incarnation-1", "delivery")).unwrap();
        let state = NativeState {
            subject: SUBJECT.into(), binding: Binding { desired_revision: "desired-1".into(), incarnation_id: "incarnation-1".into(), session_id: "session-1".into(), turn_id: Some("turn-1".into()) }, idle: false, input_supported: true, steer: Default::default(),
            models: Models { choices: vec![], selected: None, atomic_model_effort: false, revision: "models-1".into(), available: false, complete: true, source: "native-extension-model-registry".into() },
            approval: Approval { supported: false, reason: "unsupported".into() }, ask_supported: true, ask_reason: None, reason: None,
            pending_ask: Some(PendingAsk { tool_call_id: "call-one".into(), questions: vec![
                AskQuestion { id: "single".into(), question: "Choose one".into(), options: vec![AskOption { label: "A".into(), description: None, preview: None }, AskOption { label: "B".into(), description: None, preview: None }], multi: None, recommended: Some(1) },
                AskQuestion { id: "multi".into(), question: "Choose any".into(), options: vec![AskOption { label: "X".into(), description: None, preview: None }], multi: Some(true), recommended: None }
            ] }),
        };
        store.observe_harness_control(&state, &fence).unwrap();
        (store, state, fence)
    }
    fn request(state: &NativeState, key: &str) -> AskRequest {
        AskRequest { actor: "person/operator".into(), idempotency_key: format!("ask-operation-{key}"), parameters: AskParameters { subject: SUBJECT.into(), binding: state.binding.clone(), tool_call_id: "call-one".into(), answers: vec![AskAnswer { id: "single".into(), selected_options: vec!["B".into()], custom_input: None }, AskAnswer { id: "multi".into(), selected_options: vec!["X".into()], custom_input: Some("also Y".into()) }] } }
    }
    #[test]
    fn ask_admission_is_atomic_and_does_not_submit_queued_chat() {
        let (store, mut state, fence) = baseline();
        let good = request(&state, "valid");
        let mut bad = request(&state, "missing");
        bad.parameters.answers.pop();
        assert_eq!(store.reserve_harness_ask(&bad).unwrap_err().code, "invalid-harness-answers");
        bad.parameters.answers = good.parameters.answers.iter().rev().cloned().collect();
        assert_eq!(store.reserve_harness_ask(&bad).unwrap_err().code, "invalid-harness-answers");
        bad.parameters.answers = good.parameters.answers.clone(); bad.parameters.answers[0].selected_options.push("A".into());
        assert_eq!(store.reserve_harness_ask(&bad).unwrap_err().code, "invalid-harness-answers");
        state.ask_supported = false; state.ask_reason = Some("native-ask-edited-in-terminal".into());
        store.observe_harness_control(&state, &fence).unwrap();
        assert_eq!(store.reserve_harness_ask(&good).unwrap_err().code, "unsupported-harness-ask");
        state.ask_supported = true; state.ask_reason = None; store.observe_harness_control(&state, &fence).unwrap();
        let accepted = store.reserve_harness_ask(&good).unwrap();
        assert_eq!(accepted.status, Outcome::Accepted);
        assert!(store.harness_control_queue(SUBJECT).unwrap().entries.is_empty());
        assert_eq!(store.reserve_harness_ask(&request(&state, "duplicate")).unwrap_err().code, "already-settled");
        assert_eq!(store.harness_control_state(SUBJECT).unwrap().unwrap().pending_ask.unwrap().tool_call_id, "call-one");
    }
    #[test]
    fn uncertain_terminal_write_cannot_replay_and_replaced_delivery_cannot_write() {
        let (store, state, fence) = baseline();
        let receipt = store.reserve_harness_ask(&request(&state, "terminal")).unwrap();
        store.take_harness_ask(&fence).unwrap().unwrap();
        let mut input = AskTerminalInput {
            operation_id: receipt.operation_id.clone(), binding: state.binding.clone(), tool_call_id: "call-one".into(),
            token: "st-ask-00000000-0000-0000-0000-000000000001".into(), question_index: 0,
            surface: st3_schema::harness_control::AskSurface::Question,
        };
        let transport = store.send_harness_ask_token(&input, &fence, || Err(anyhow::anyhow!("write acknowledgement lost"))).unwrap();
        assert!(transport.is_err());
        assert_eq!(store.send_harness_ask_token(&input, &fence, || panic!("uncertain input must never replay")).unwrap_err().code, "already-dispatched");
        assert_eq!(store.harness_ask_receipt(&receipt.operation_id).unwrap().unwrap().status, Outcome::Dispatched);
        let replacement = store.bind_mailbox(&crate::mailbox::Fence::new(SUBJECT, "incarnation-1", "delivery")).unwrap();
        input.token = "st-ask-00000000-0000-0000-0000-000000000002".into();
        assert_eq!(store.send_harness_ask_token(&input, &fence, || panic!("replaced delivery must never write")).unwrap_err().code, "stale-mailbox-session");
        assert!(store.send_harness_ask_token(&input, &replacement, || Ok(())).unwrap().is_ok());
    }
    #[test]
    fn terminal_conflict_never_dispatches_or_fabricates_native_answers() {
        let (store, mut state, fence) = baseline();
        state.ask_supported = false; state.ask_reason = Some("terminal-input-conflict".into());
        store.observe_harness_control(&state, &fence).unwrap();
        let receipt = store.reserve_harness_ask(&request(&state, "before-admission")).unwrap();
        assert_eq!(receipt.status, Outcome::Indeterminate);
        assert_eq!(receipt.outcome, Some(AskOutcome::Indeterminate { reason: AskIndeterminateReason::TerminalInputConflict }));
        assert!(store.take_harness_ask(&fence).unwrap().is_none());
        assert!(receipt.result.is_none());
        assert_eq!(store.harness_control_state(SUBJECT).unwrap().unwrap().pending_ask.unwrap().tool_call_id, "call-one");
    }
    #[test]
    fn dispatched_terminal_conflict_is_indeterminate_and_keeps_native_ask_pending() {
        let (store, state, fence) = baseline();
        let accepted = store.reserve_harness_ask(&request(&state, "conflict")).unwrap();
        let command = store.take_harness_ask(&fence).unwrap().unwrap();
        let native = NativeReceipt {
            subject: SUBJECT.into(), operation_id: accepted.operation_id.clone(), binding: command.binding,
            status: Outcome::Indeterminate, reason: Some("terminal-input-conflict".into()), result: None,
        };
        let receipt = store.settle_harness_ask(&native, &fence).unwrap();
        assert_eq!(receipt.status, Outcome::Indeterminate);
        assert_eq!(receipt.outcome, Some(AskOutcome::Indeterminate { reason: AskIndeterminateReason::TerminalInputConflict }));
        assert!(receipt.result.is_none());
        assert_eq!(store.reserve_harness_ask(&request(&state, "retry-conflict")).unwrap_err().code, "already-settled");
        assert_eq!(store.harness_control_state(SUBJECT).unwrap().unwrap().pending_ask.unwrap().tool_call_id, "call-one");
    }
    #[test]
    fn only_matching_native_result_settles_and_stale_answer_cannot_touch_new_ask() {
        let (store, mut state, fence) = baseline();
        let request = request(&state, "settle");
        let accepted = store.reserve_harness_ask(&request).unwrap();
        let command = store.take_harness_ask(&fence).unwrap().unwrap();
        assert!(store.take_harness_ask(&fence).unwrap().is_none());
        let mut native = NativeReceipt { subject: SUBJECT.into(), operation_id: accepted.operation_id.clone(), binding: command.binding.clone(), status: Outcome::Applied, reason: None, result: Some(NativeResult::Ask(AskResult { native_event: "tool_result".into(), tool_call_id: "other-call".into(), answers: command.answers.clone() })) };
        assert_eq!(store.settle_harness_ask(&native, &fence).unwrap_err().code, "invalid-native-receipt");
        assert_eq!(store.harness_ask_receipt(&accepted.operation_id).unwrap().unwrap().status, Outcome::Dispatched);
        native.result = Some(NativeResult::Ask(AskResult { native_event: "tool_result".into(), tool_call_id: command.tool_call_id, answers: command.answers }));
        let applied = store.settle_harness_ask(&native, &fence).unwrap();
        assert_eq!(applied.status, Outcome::Applied);
        assert!(store.harness_control_state(SUBJECT).unwrap().unwrap().pending_ask.is_some(), "only native observation clears the ask, not an action receipt");
        state.pending_ask.as_mut().unwrap().tool_call_id = "call-two".into();
        store.observe_harness_control(&state, &fence).unwrap();
        let mut stale = request.clone(); stale.idempotency_key = "ask-operation-stale".into();
        assert_eq!(store.reserve_harness_ask(&stale).unwrap_err().code, "stale-harness-ask");
        assert_eq!(store.harness_control_state(SUBJECT).unwrap().unwrap().pending_ask.unwrap().tool_call_id, "call-two");
        assert_eq!(store.reserve_harness_ask(&request).unwrap(), applied);
    }
}
