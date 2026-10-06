//! Owner-local model operations. The input dispatcher owns the shared lane schema;
//! accepted model changes retain that same lane until settlement or invalidation.
//! A dispatched command is never replayed, including after an owner restart.
use super::*;
use super::harness_control::{check_binding, check_runtime, release_dispatch_tx, reserve_dispatch_tx, same_session, state_tx};
use st3_schema::harness_control::{ModelCommand, ModelParameters, ModelReceipt, ModelRequest, NativeReceipt, NativeResult, Outcome};

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS local_harness_model_operations(
        id TEXT PRIMARY KEY, subject TEXT NOT NULL, digest TEXT NOT NULL,
        parameters TEXT NOT NULL, receipt TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS local_harness_model_subject ON local_harness_model_operations(subject);")?;
    Ok(())
}

fn receipt_tx(connection: &Connection, operation: &str) -> Result<Option<ModelReceipt>, St3Error> {
    let stored: Option<String> = connection.query_row(
        "SELECT receipt FROM local_harness_model_operations WHERE id=?1", [operation], |row| row.get(0)
    ).optional().map_err(internal)?;
    stored.map(|value| serde_json::from_str(&value).map_err(internal)).transpose()
}

fn save_receipt(connection: &Connection, receipt: &ModelReceipt) -> Result<(), St3Error> {
    connection.execute(
        "UPDATE local_harness_model_operations SET receipt=?2 WHERE id=?1 AND json_extract(receipt,'$.status') IN ('accepted','dispatched')",
        params![receipt.operation_id, serde_json::to_string(receipt).map_err(internal)?]
    ).map_err(internal)?;
    Ok(())
}

/// Called inside the input owner's replacement/close transaction. Terminal native
/// evidence is frozen; only an undispatched reservation can honestly be rejected.
pub(super) fn invalidate_binding_tx(connection: &Connection, subject: &str, reason: &str) -> Result<(), St3Error> {
    let mut statement = connection.prepare(
        "SELECT receipt FROM local_harness_model_operations WHERE subject=?1 AND json_extract(receipt,'$.status') IN ('accepted','dispatched')"
    ).map_err(internal)?;
    let receipts = statement.query_map([subject], |row| row.get::<_, String>(0)).map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(internal)?;
    drop(statement);
    for stored in receipts {
        let mut receipt: ModelReceipt = serde_json::from_str(&stored).map_err(internal)?;
        receipt.status = if receipt.status == Outcome::Dispatched { Outcome::Indeterminate } else { Outcome::Rejected };
        receipt.reason = Some(reason.into());
        save_receipt(connection, &receipt)?;
        release_dispatch_tx(connection, subject, &receipt.operation_id)?;
    }
    Ok(())
}

fn reconcile_runtime(connection: &Connection, subject: &str) -> Result<(), St3Error> {
    let Some(state) = state_tx(connection, subject)? else {
        return invalidate_binding_tx(connection, subject, "native-binding-unavailable");
    };
    if let Err(error) = check_runtime(connection, subject, &state.binding) {
        if !matches!(error.code, "stale-harness-control" | "stale-mailbox-session") { return Err(error); }
        invalidate_binding_tx(connection, subject, "native-runtime-ended-or-replaced")?;
    }
    Ok(())
}

fn validate_parameters(connection: &Connection, parameters: &ModelParameters) -> Result<(), St3Error> {
    let state = check_binding(connection, &parameters.subject, &parameters.binding)?;
    if state.reason.as_deref() == Some("session-transition-state-unknown") {
        return Err(St3Error::new("unsupported-harness-model", "the native transition state is unknown"));
    }
    if !state.models.available || !state.models.complete || state.models.revision.is_empty() {
        return Err(St3Error::new("unsupported-harness-model", "the native model catalog is unavailable or incomplete"));
    }
    if state.models.revision != parameters.model_revision {
        return Err(St3Error::new("stale-harness-model", "the native model catalog or selection changed"));
    }
    let choice = state.models.choices.iter().find(|choice| choice.provider == parameters.provider && choice.id == parameters.model_id)
        .ok_or_else(|| St3Error::new("unavailable-harness-model", "the requested model is not in the available native catalog"))?;
    if let Some(effort) = &parameters.effort
        && (!choice.reasoning || !choice.supported_efforts.contains(effort))
    {
        return Err(St3Error::new("unsupported-harness-effort", "the requested effort is not advertised for this native model"));
    }
    Ok(())
}

impl Store {
    /// Concrete-person identity is supplied by the authenticated action boundary.
    /// Fences and native revisions are admission preconditions, not semantic input:
    /// retrying the same selection recovers its durable receipt, never a new send.
    pub fn reserve_harness_model(&self, request: &ModelRequest) -> Result<ModelReceipt, St3Error> {
        if !request.actor.starts_with("person/") || request.actor.split('/').count() != 2 || request.actor == "person/" {
            return Err(St3Error::new("forbidden", "harness model changes require a concrete person"));
        }
        if !(16..=256).contains(&request.idempotency_key.len()) {
            return Err(St3Error::new("invalid-idempotency-key", "idempotency key must contain 16 to 256 bytes"));
        }
        let operation = format!("operation/harness-model-{}", hex::encode(Sha256::digest(
            serde_json::to_vec(&(&request.actor, &request.idempotency_key)).map_err(internal)?
        )));
        let parameters = &request.parameters;
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&(
            &parameters.subject, &request.actor, &parameters.provider, &parameters.model_id, &parameters.effort
        )).map_err(internal)?));
        self.connection.batched(|tx| -> Result<ModelReceipt, St3Error> {
            reconcile_runtime(tx, &parameters.subject)?;
            let prior: Option<(String, String)> = tx.query_row(
                "SELECT digest,receipt FROM local_harness_model_operations WHERE id=?1", [&operation], |row| Ok((row.get(0)?, row.get(1)?))
            ).optional().map_err(internal)?;
            if let Some((stored_digest, stored_receipt)) = prior {
                if stored_digest != digest {
                    return Err(St3Error::new("idempotency-conflict", "this operation key identifies a different model change"));
                }
                return serde_json::from_str(&stored_receipt).map_err(internal);
            }
            validate_parameters(tx, parameters)?;
            if !reserve_dispatch_tx(tx, &parameters.subject, &operation, "set_model", &parameters.binding)? {
                return Err(St3Error::new("harness-control-busy", "an unresolved native input or model change owns the control lane"));
            }
            let receipt = ModelReceipt {
                operation_id: operation.clone(), subject: parameters.subject.clone(), binding: parameters.binding.clone(),
                model_revision: parameters.model_revision.clone(), status: Outcome::Accepted, reason: None, result: None
            };
            tx.execute(
                "INSERT INTO local_harness_model_operations(id,subject,digest,parameters,receipt) VALUES(?1,?2,?3,?4,?5)",
                params![operation, parameters.subject, digest, serde_json::to_string(parameters).map_err(internal)?, serde_json::to_string(&receipt).map_err(internal)?]
            ).map_err(internal)?;
            Ok(receipt)
        }).map_err(|error| St3Error::new("internal", error))?
    }

    /// The persisted accepted -> dispatched transition precedes returning command
    /// bytes. Reopening the database cannot resend an uncertain native mutation.
    pub fn take_harness_model(&self, fence: &crate::mailbox::Fence) -> Result<Option<ModelCommand>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<ModelCommand>, St3Error> {
            check_mailbox_fence(tx, fence)?;
            reconcile_runtime(tx, &fence.subject)?;
            let operation: Option<String> = tx.query_row(
                "SELECT operation_id FROM local_harness_control_dispatch WHERE subject=?1 AND kind='set_model'", [&fence.subject], |row| row.get(0)
            ).optional().map_err(internal)?;
            let Some(operation) = operation else { return Ok(None); };
            let mut receipt = receipt_tx(tx, &operation)?.ok_or_else(|| St3Error::new("missing-harness-operation", "reserved native model operation does not exist"))?;
            if receipt.status != Outcome::Accepted { return Ok(None); }
            let stored: String = tx.query_row("SELECT parameters FROM local_harness_model_operations WHERE id=?1", [&operation], |row| row.get(0)).map_err(internal)?;
            let parameters: ModelParameters = serde_json::from_str(&stored).map_err(internal)?;
            if let Err(error) = validate_parameters(tx, &parameters) {
                if !matches!(error.code, "stale-harness-control" | "stale-mailbox-session" | "unsupported-harness-control" | "unsupported-harness-model" | "stale-harness-model" | "unavailable-harness-model" | "unsupported-harness-effort") { return Err(error); }
                receipt.status = Outcome::Rejected;
                receipt.reason = Some(error.code.to_owned());
                save_receipt(tx, &receipt)?;
                release_dispatch_tx(tx, &fence.subject, &operation)?;
                return Ok(None);
            }
            receipt.status = Outcome::Dispatched;
            save_receipt(tx, &receipt)?;
            Ok(Some(ModelCommand {
                operation_id: operation, binding: parameters.binding, model_revision: parameters.model_revision,
                provider: parameters.provider, model_id: parameters.model_id, effort: parameters.effort
            }))
        }).map_err(|error| St3Error::new("internal", error))?
    }

    /// Only exact native evidence settles a model command. In particular, a
    /// requested effort never becomes an effective effort without native proof,
    /// and a partially applied non-atomic change retains its indeterminate result.
    pub fn settle_harness_model(&self, native: &NativeReceipt, fence: &crate::mailbox::Fence) -> Result<ModelReceipt, St3Error> {
        if !matches!(native.status, Outcome::Applied | Outcome::Rejected | Outcome::Indeterminate) {
            return Err(St3Error::new("invalid-native-receipt", "native settlement must be applied, rejected, or indeterminate"));
        }
        let result = match &native.result {
            Some(NativeResult::Model(result)) if !result.atomic_model_effort => Some(result),
            None if native.status != Outcome::Applied => None,
            _ => return Err(St3Error::new("invalid-native-receipt", "model settlement requires native model evidence without an atomic model/effort claim"))
        };
        self.connection.batched(|tx| -> Result<ModelReceipt, St3Error> {
            check_mailbox_fence(tx, fence)?;
            if native.subject != fence.subject || native.binding.incarnation_id != fence.incarnation {
                return Err(St3Error::new("foreign-harness-control", "receipt belongs to another runtime"));
            }
            let mut receipt = receipt_tx(tx, &native.operation_id)?.ok_or_else(|| St3Error::new("missing-harness-operation", "native model operation does not exist"))?;
            if native.subject != receipt.subject || native.binding != receipt.binding {
                return Err(St3Error::new("stale-harness-control", "native receipt does not match the reserved model operation"));
            }
            if receipt.status != Outcome::Dispatched {
                if receipt.status == native.status && receipt.reason == native.reason && receipt.result.as_ref() == result {
                    return Ok(receipt);
                }
                return Err(St3Error::new("already-settled", "native model operation is not awaiting settlement"));
            }
            check_runtime(tx, &native.subject, &native.binding)?;
            let current = state_tx(tx, &native.subject)?.ok_or_else(|| St3Error::new("stale-harness-control", "native binding is no longer available"))?;
            if !same_session(&current.binding, &native.binding) {
                return Err(St3Error::new("stale-harness-control", "native session changed before model settlement"));
            }
            let lane: Option<(String, String, String)> = tx.query_row(
                "SELECT operation_id,kind,binding FROM local_harness_control_dispatch WHERE subject=?1", [&native.subject], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            ).optional().map_err(internal)?;
            if !matches!(lane, Some((operation, kind, binding)) if operation == native.operation_id && kind == "set_model" && serde_json::from_str::<st3_schema::harness_control::Binding>(&binding).map_err(internal)? == native.binding) {
                return Err(St3Error::new("stale-harness-control", "native model operation no longer owns the control lane"));
            }
            if native.status == Outcome::Applied {
                let stored: String = tx.query_row("SELECT parameters FROM local_harness_model_operations WHERE id=?1", [&native.operation_id], |row| row.get(0)).map_err(internal)?;
                let parameters: ModelParameters = serde_json::from_str(&stored).map_err(internal)?;
                // Dispatch checked the catalog revision. Native credential awaits
                // are not an atomic CAS: settlement checks actual effects instead
                // of requiring the now possibly changed catalog revision.
                if !matches!(result, Some(result) if result.provider.as_deref() == Some(parameters.provider.as_str()) && result.id.as_deref() == Some(parameters.model_id.as_str()) && parameters.effort.as_ref().is_none_or(|effort| result.effective_effort.as_ref() == Some(effort))) {
                    return Err(St3Error::new("invalid-native-receipt", "applied model change requires the exact native model and requested effective effort"));
                }
            }
            receipt.status = native.status;
            receipt.reason.clone_from(&native.reason);
            receipt.result = result.cloned();
            save_receipt(tx, &receipt)?;
            release_dispatch_tx(tx, &native.subject, &native.operation_id)?;
            Ok(receipt)
        }).map_err(|error| St3Error::new("internal", error))?
    }

    pub fn harness_model_receipt(&self, operation: &str) -> Result<Option<ModelReceipt>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<ModelReceipt>, St3Error> {
            let Some(receipt) = receipt_tx(tx, operation)? else { return Ok(None); };
            if matches!(receipt.status, Outcome::Accepted | Outcome::Dispatched) {
                reconcile_runtime(tx, &receipt.subject)?;
                return receipt_tx(tx, operation);
            }
            Ok(Some(receipt))
        }).map_err(|error| St3Error::new("internal", error))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st3_schema::harness_control::{Approval, Binding, InputResult, Lane, ModelChoice, ModelResult, Models, NativeState, QueueMutation, QueueRequest, SelectedModel};

    const SUBJECT: &str = "agent/model-owner.model-control";
    fn baseline(store: &Store) -> (NativeState, crate::mailbox::Fence) {
        let intent = crate::graph::parse_intent("version 2\nagent \"model-control\" { workspace \".\"; harness \"omp\" { model \"provider/reasoner\"; } }", "model-owner").unwrap();
        let planned = store.mission(&intent, crate::model::IntentInput { kdl: String::new(), source_name: None }).unwrap();
        store.apply_as(&intent, &planned.subject_tokens, "model-test-declaration", Some("person/operator")).unwrap();
        store.append_claim(&ClaimInput {
            subject: SUBJECT.into(), kind: "runtime.observed".into(), actor: Some(SUBJECT.into()),
            fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!("incarnation-1")), ("runtime_id".into(), json!("native-runtime"))]),
            evidence: Vec::new(), expected_subject: None, idempotency_key: None
        }).unwrap();
        let fence = store.bind_mailbox(&crate::mailbox::Fence::new(SUBJECT, "incarnation-1", "delivery")).unwrap();
        let state = NativeState {
            subject: SUBJECT.into(), binding: Binding { desired_revision: store.harness_control_desired_revision(&fence).unwrap(), incarnation_id: "incarnation-1".into(), session_id: "session-1".into(), turn_id: Some("turn-1".into()) }, idle: true, input_supported: true,
            steer: Default::default(),
            models: Models {
                choices: vec![
                    ModelChoice { provider: "provider".into(), id: "reasoner".into(), reasoning: true, supported_efforts: vec!["medium".into(), "high".into()] },
                    ModelChoice { provider: "provider".into(), id: "plain".into(), reasoning: false, supported_efforts: Vec::new() }
                ],
                selected: Some(SelectedModel { provider: "provider".into(), id: "plain".into(), effective_effort: None, configured_effort: "medium".into() }),
                atomic_model_effort: false, revision: "models-1".into(), available: true, complete: true, source: "native-extension-model-registry".into()
            },
            approval: Approval { supported: false, reason: "native-live-approval-api-unavailable".into() }, reason: None
        };
        store.observe_harness_control(&state, &fence).unwrap();
        (state, fence)
    }
    fn request(state: &NativeState, key: &str) -> ModelRequest {
        ModelRequest { actor: "person/operator".into(), idempotency_key: format!("model-operation-{key}"), parameters: ModelParameters {
            subject: SUBJECT.into(), binding: state.binding.clone(), model_revision: state.models.revision.clone(), provider: "provider".into(), model_id: "reasoner".into(), effort: Some("high".into())
        } }
    }
    fn model_proof(command: &ModelCommand, status: Outcome, result: Option<ModelResult>) -> NativeReceipt {
        NativeReceipt { subject: SUBJECT.into(), binding: command.binding.clone(), operation_id: command.operation_id.clone(), status, reason: None, result: result.map(NativeResult::Model) }
    }
    fn actual_model(id: &str, effort: Option<&str>) -> ModelResult {
        ModelResult { provider: Some("provider".into()), id: Some(id.into()), effective_effort: effort.map(str::to_owned), atomic_model_effort: false }
    }
    fn enqueue(store: &Store, state: &NativeState, key: &str) {
        store.mutate_harness_queue(&QueueRequest {
            subject: SUBJECT.into(), actor: "person/operator".into(), idempotency_key: format!("input-operation-{key}"), binding: state.binding.clone(),
            queue_revision: store.harness_control_queue(SUBJECT).unwrap().revision,
            mutation: QueueMutation::Enqueue { content: "real pending input".into(), lane: Lane::FollowUp }
        }).unwrap();
    }

    #[test]
    fn stale_binding_and_catalog_reject_before_reservation_and_take() {
        let store = Store::open_memory("model-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        for field in ["desired", "incarnation", "session", "turn", "catalog"] {
            let mut stale = request(&state, field);
            match field {
                "desired" => stale.parameters.binding.desired_revision = "old".into(),
                "incarnation" => stale.parameters.binding.incarnation_id = "old".into(),
                "session" => stale.parameters.binding.session_id = "old".into(),
                "turn" => stale.parameters.binding.turn_id = None,
                _ => stale.parameters.model_revision = "old".into()
            }
            assert!(matches!(store.reserve_harness_model(&stale).unwrap_err().code, "stale-harness-control" | "stale-mailbox-session" | "stale-harness-model"));
        }
        let accepted = store.reserve_harness_model(&request(&state, "catalog-changes")).unwrap();
        state.models.revision = "models-2".into();
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        let rejected = store.harness_model_receipt(&accepted.operation_id).unwrap().unwrap();
        assert_eq!(rejected.status, Outcome::Rejected);
        assert_eq!(rejected.reason.as_deref(), Some("stale-harness-model"));
        let accepted = store.reserve_harness_model(&request(&state, "turn-changes")).unwrap();
        state.binding.turn_id = Some("turn-2".into());
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.harness_model_receipt(&accepted.operation_id).unwrap().unwrap().status, Outcome::Rejected);
    }

    #[test]
    fn catalog_support_not_requested_effort_controls_admission() {
        let store = Store::open_memory("model-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        let mut unsupported = request(&state, "unsupported");
        unsupported.parameters.effort = Some("max".into());
        assert_eq!(store.reserve_harness_model(&unsupported).unwrap_err().code, "unsupported-harness-effort");
        unsupported.parameters.model_id = "plain".into();
        unsupported.parameters.effort = Some("high".into());
        assert_eq!(store.reserve_harness_model(&unsupported).unwrap_err().code, "unsupported-harness-effort");
        unsupported.parameters.model_id = "missing".into();
        assert_eq!(store.reserve_harness_model(&unsupported).unwrap_err().code, "unavailable-harness-model");
        state.models.available = false;
        store.observe_harness_control(&state, &fence).unwrap();
        assert_eq!(store.reserve_harness_model(&request(&state, "unavailable")).unwrap_err().code, "unsupported-harness-model");
        state.models.available = true;
        state.models.complete = false;
        store.observe_harness_control(&state, &fence).unwrap();
        assert_eq!(store.reserve_harness_model(&request(&state, "incomplete")).unwrap_err().code, "unsupported-harness-model");
        state.models.complete = true;
        store.observe_harness_control(&state, &fence).unwrap();
        unsupported.parameters.model_id = "plain".into();
        unsupported.parameters.effort = None;
        store.reserve_harness_model(&unsupported).unwrap();
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        let settled = store.settle_harness_model(&model_proof(&command, Outcome::Applied, Some(actual_model("plain", None))), &fence).unwrap();
        assert_eq!(settled.result.unwrap().effective_effort, None);
    }

    #[test]
    fn model_capability_is_independent_of_input_but_unknown_transition_blocks_admission() {
        let store = Store::open_memory("model-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        state.input_supported = false;
        store.observe_harness_control(&state, &fence).unwrap();
        let original = request(&state, "without-input");
        store.reserve_harness_model(&original).unwrap();
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        store.settle_harness_model(&model_proof(&command, Outcome::Applied, Some(actual_model("reasoner", Some("high")))), &fence).unwrap();
        state.reason = Some("session-transition-state-unknown".into());
        store.observe_harness_control(&state, &fence).unwrap();
        assert_eq!(store.reserve_harness_model(&request(&state, "unknown-transition")).unwrap_err().code, "unsupported-harness-model");
        state.reason = None;
        store.observe_harness_control(&state, &fence).unwrap();
        let pending = store.reserve_harness_model(&request(&state, "becomes-unknown")).unwrap();
        state.reason = Some("session-transition-state-unknown".into());
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.harness_model_receipt(&pending.operation_id).unwrap().unwrap().status, Outcome::Rejected);
    }

    #[test]
    fn take_rechecks_actual_choice_and_effort_and_never_infers_unrequested_effort() {
        let store = Store::open_memory("model-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        let original_choices = state.models.choices.clone();
        let pending = store.reserve_harness_model(&request(&state, "choice-removed")).unwrap();
        state.models.choices.retain(|choice| choice.id != "reasoner");
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.harness_model_receipt(&pending.operation_id).unwrap().unwrap().reason.as_deref(), Some("unavailable-harness-model"));
        state.models.choices = original_choices;
        store.observe_harness_control(&state, &fence).unwrap();
        let pending = store.reserve_harness_model(&request(&state, "effort-removed")).unwrap();
        state.models.choices[0].supported_efforts.retain(|effort| effort != "high");
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.harness_model_receipt(&pending.operation_id).unwrap().unwrap().reason.as_deref(), Some("unsupported-harness-effort"));
        let mut without_effort = request(&state, "keep-native-effort");
        without_effort.parameters.effort = None;
        store.reserve_harness_model(&without_effort).unwrap();
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        let settled = store.settle_harness_model(&model_proof(&command, Outcome::Applied, Some(actual_model("reasoner", Some("medium")))), &fence).unwrap();
        assert_eq!(settled.result.unwrap().effective_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn actor_scoped_keys_conflict_only_on_changed_semantic_selection() {
        let store = Store::open_memory("model-owner").unwrap();
        let (state, fence) = baseline(&store);
        let original = request(&state, "same-key");
        let accepted = store.reserve_harness_model(&original).unwrap();
        let mut retry = original.clone();
        retry.parameters.binding.session_id = "later-session".into();
        retry.parameters.model_revision = "later-catalog".into();
        assert_eq!(store.reserve_harness_model(&retry).unwrap(), accepted);
        for field in ["provider", "model", "effort", "subject"] {
            let mut conflict = original.clone();
            match field {
                "provider" => conflict.parameters.provider = "other".into(),
                "model" => conflict.parameters.model_id = "plain".into(),
                "effort" => conflict.parameters.effort = None,
                _ => conflict.parameters.subject = "agent/other".into()
            }
            assert_eq!(store.reserve_harness_model(&conflict).unwrap_err().code, "idempotency-conflict");
        }
        let mut invalid_actor = original.clone();
        invalid_actor.actor = "agent/operator".into();
        assert_eq!(store.reserve_harness_model(&invalid_actor).unwrap_err().code, "forbidden");
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        store.settle_harness_model(&model_proof(&command, Outcome::Rejected, None), &fence).unwrap();
        let mut other_actor = original;
        other_actor.actor = "person/operator-two".into();
        assert_ne!(store.reserve_harness_model(&other_actor).unwrap().operation_id, accepted.operation_id);
    }

    #[test]
    fn input_and_model_share_one_unresolved_native_lane() {
        let store = Store::open_memory("model-owner").unwrap();
        let (state, fence) = baseline(&store);
        enqueue(&store, &state, "first");
        let input = store.take_harness_input(SUBJECT, &fence).unwrap().unwrap();
        let change = request(&state, "blocked-by-input");
        assert_eq!(store.reserve_harness_model(&change).unwrap_err().code, "harness-control-busy");
        store.settle_harness_input(&NativeReceipt {
            subject: SUBJECT.into(), binding: input.binding, operation_id: input.operation_id,
            status: Outcome::Applied, reason: None,
            result: Some(NativeResult::Input(InputResult { native_event: "message_start".into(), turn_id: Some("turn-1".into()) }))
        }, &fence).unwrap();
        store.reserve_harness_model(&change).unwrap();
        enqueue(&store, &state, "second");
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none());
        assert_eq!(store.reserve_harness_model(&request(&state, "another-model")).unwrap_err().code, "harness-control-busy");
        let model = store.take_harness_model(&fence).unwrap().unwrap();
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none());
        store.settle_harness_model(&model_proof(&model, Outcome::Rejected, None), &fence).unwrap();
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_some());
    }

    #[test]
    fn exact_native_results_freeze_and_partial_effects_remain_indeterminate() {
        let store = Store::open_memory("model-owner").unwrap();
        let (state, fence) = baseline(&store);
        let original = request(&state, "native-result");
        store.reserve_harness_model(&original).unwrap();
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        let proof = model_proof(&command, Outcome::Applied, Some(actual_model("reasoner", Some("high"))));
        let mut forged = proof.clone();
        forged.binding.turn_id = Some("another-turn".into());
        assert_eq!(store.settle_harness_model(&forged, &fence).unwrap_err().code, "stale-harness-control");
        for result in [None, Some(actual_model("plain", None)), Some(actual_model("reasoner", Some("medium"))), Some(ModelResult { atomic_model_effort: true, ..actual_model("reasoner", Some("high")) })] {
            forged = model_proof(&command, Outcome::Applied, result);
            assert_eq!(store.settle_harness_model(&forged, &fence).unwrap_err().code, "invalid-native-receipt");
        }
        forged = proof.clone();
        forged.result = Some(NativeResult::Input(InputResult { native_event: "message_start".into(), turn_id: None }));
        assert_eq!(store.settle_harness_model(&forged, &fence).unwrap_err().code, "invalid-native-receipt");
        let settled = store.settle_harness_model(&proof, &fence).unwrap();
        assert_eq!(settled.status, Outcome::Applied);
        assert_eq!(settled.result, Some(actual_model("reasoner", Some("high"))));
        assert_eq!(store.settle_harness_model(&proof, &fence).unwrap(), settled);
        assert_eq!(store.reserve_harness_model(&original).unwrap(), settled);
        assert_eq!(store.harness_control_state(SUBJECT).unwrap().unwrap().models.selected.unwrap().id, "plain", "receipt proof does not invent a new native observation");
        forged = model_proof(&command, Outcome::Indeterminate, Some(actual_model("reasoner", Some("medium"))));
        assert_eq!(store.settle_harness_model(&forged, &fence).unwrap_err().code, "already-settled");
        store.reserve_harness_model(&request(&state, "partial-result")).unwrap();
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        let mut partial = model_proof(&command, Outcome::Indeterminate, Some(actual_model("reasoner", Some("medium"))));
        partial.reason = Some("native-effort-changed-during-credential-await".into());
        let settled = store.settle_harness_model(&partial, &fence).unwrap();
        assert_eq!(settled.status, Outcome::Indeterminate);
        assert_eq!(settled.result, Some(actual_model("reasoner", Some("medium"))));
        store.close_harness_control(&fence).unwrap();
        assert_eq!(store.harness_model_receipt(&settled.operation_id).unwrap().unwrap(), settled);
    }

    #[test]
    fn reopen_replacement_and_close_never_replay_or_rewrite_terminal_receipts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owner.sqlite");
        let store = Store::open(&path, "model-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        let original = request(&state, "durable");
        let accepted = store.reserve_harness_model(&original).unwrap();
        store.take_harness_model(&fence).unwrap().unwrap();
        drop(store);
        let store = Store::open(&path, "model-owner").unwrap();
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.reserve_harness_model(&original).unwrap().status, Outcome::Dispatched);
        state.binding.session_id = "session-2".into();
        store.observe_harness_control(&state, &fence).unwrap();
        let frozen = store.harness_model_receipt(&accepted.operation_id).unwrap().unwrap();
        assert_eq!(frozen.status, Outcome::Indeterminate);
        assert_eq!(frozen.reason.as_deref(), Some("native-binding-replaced"));
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        assert_eq!(store.reserve_harness_model(&original).unwrap(), frozen);
        let pending = store.reserve_harness_model(&request(&state, "close-pending")).unwrap();
        store.close_harness_control(&fence).unwrap();
        assert_eq!(store.harness_model_receipt(&pending.operation_id).unwrap().unwrap().status, Outcome::Rejected);
        assert_eq!(store.harness_model_receipt(&frozen.operation_id).unwrap().unwrap(), frozen);
        store.observe_harness_control(&state, &fence).unwrap();
        let dispatched = store.reserve_harness_model(&request(&state, "close-dispatched")).unwrap();
        store.take_harness_model(&fence).unwrap().unwrap();
        store.close_harness_control(&fence).unwrap();
        assert_eq!(store.harness_model_receipt(&dispatched.operation_id).unwrap().unwrap().status, Outcome::Indeterminate);
    }

    #[test]
    fn undispatched_reservation_survives_reopen_but_dispatch_remains_one_shot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owner.sqlite");
        let store = Store::open(&path, "model-owner").unwrap();
        let (state, fence) = baseline(&store);
        let original = request(&state, "pending-reopen");
        let accepted = store.reserve_harness_model(&original).unwrap();
        drop(store);
        let store = Store::open(&path, "model-owner").unwrap();
        assert_eq!(store.reserve_harness_model(&original).unwrap(), accepted);
        let command = store.take_harness_model(&fence).unwrap().unwrap();
        assert_eq!(command.operation_id, accepted.operation_id);
        assert!(store.take_harness_model(&fence).unwrap().is_none());
        let proof = model_proof(&command, Outcome::Applied, Some(actual_model("reasoner", Some("high"))));
        let settled = store.settle_harness_model(&proof, &fence).unwrap();
        drop(store);
        let store = Store::open(&path, "model-owner").unwrap();
        assert_eq!(store.reserve_harness_model(&original).unwrap(), settled);
        assert!(store.take_harness_model(&fence).unwrap().is_none());
    }

    #[test]
    fn declaration_replacement_is_reconciled_by_receipt_inspection() {
        let store = Store::open_memory("model-owner").unwrap();
        let (state, fence) = baseline(&store);
        let accepted = store.reserve_harness_model(&request(&state, "declaration")).unwrap();
        store.take_harness_model(&fence).unwrap().unwrap();
        let replacement = crate::graph::parse_intent("version 2\nagent \"model-control\" { workspace \".\"; harness \"omp\" { model \"provider/plain\"; } }", "model-owner").unwrap();
        let planned = store.mission(&replacement, crate::model::IntentInput { kdl: String::new(), source_name: None }).unwrap();
        store.apply_as(&replacement, &planned.subject_tokens, "model-test-replacement", Some("person/operator")).unwrap();
        let frozen = store.harness_model_receipt(&accepted.operation_id).unwrap().unwrap();
        assert_eq!(frozen.status, Outcome::Indeterminate);
        assert_eq!(frozen.reason.as_deref(), Some("native-runtime-ended-or-replaced"));
        assert_eq!(store.harness_model_receipt(&accepted.operation_id).unwrap().unwrap(), frozen);
    }
}
