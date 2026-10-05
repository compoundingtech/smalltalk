//! The local runtime owner holds pending input. Dispatch is one-shot: native process loss
//! cannot turn an uncertain handoff into another prompt.
use super::*;
use st3_schema::harness_control::{Binding, InputCommand, Lane, NativeReceipt, NativeState, Outcome, Queue, QueueEntry, QueueMutation, QueueRequest, Receipt};

const MAX_PENDING: usize = 128;
const MAX_CONTENT: usize = 64 * 1024;

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS local_harness_control_state(subject TEXT PRIMARY KEY, state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS local_harness_control_queue(subject TEXT PRIMARY KEY, queue TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS local_harness_control_operations(id TEXT PRIMARY KEY, subject TEXT NOT NULL, digest TEXT NOT NULL, receipt TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS local_harness_control_dispatch(subject TEXT PRIMARY KEY, operation_id TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('input','set_model')), binding TEXT NOT NULL);")?;
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(value: Option<String>) -> Result<Option<T>, St3Error> {
    value.map(|value| serde_json::from_str(&value).map_err(internal)).transpose()
}
pub(super) fn state_tx(connection: &Connection, subject: &str) -> Result<Option<NativeState>, St3Error> {
    decode(connection.query_row("SELECT state FROM local_harness_control_state WHERE subject=?1", [subject], |row| row.get(0)).optional().map_err(internal)?)
}
fn queue_tx(connection: &Connection, subject: &str) -> Result<Queue, St3Error> {
    Ok(decode(connection.query_row("SELECT queue FROM local_harness_control_queue WHERE subject=?1", [subject], |row| row.get(0)).optional().map_err(internal)?)?.unwrap_or_default())
}
fn save_queue(connection: &Connection, subject: &str, queue: &mut Queue) -> Result<(), St3Error> {
    queue.entries.retain(|entry| matches!(entry.status, Outcome::Accepted | Outcome::Dispatched));
    connection.execute("INSERT INTO local_harness_control_queue(subject,queue) VALUES(?1,?2) ON CONFLICT(subject) DO UPDATE SET queue=excluded.queue", params![subject, serde_json::to_string(queue).map_err(internal)?]).map_err(internal)?;
    Ok(())
}
fn save_receipt(connection: &Connection, receipt: &Receipt) -> Result<(), St3Error> {
    connection.execute("UPDATE local_harness_control_operations SET receipt=?2 WHERE id=?1 AND json_extract(receipt,'$.status') IN ('accepted','dispatched')", params![receipt.operation_id, serde_json::to_string(receipt).map_err(internal)?]).map_err(internal)?;
    Ok(())
}
fn receipt_for(entry: &QueueEntry, subject: &str, revision: u64) -> Receipt {
    Receipt { operation_id: entry.operation_id.clone(), subject: subject.into(), entry_id: Some(entry.id.clone()), status: entry.status, queue_revision: revision, reason: entry.reason.clone(), result: entry.result.clone(), binding: entry.binding.clone() }
}
pub(super) fn same_session(left: &Binding, right: &Binding) -> bool {
    left.desired_revision == right.desired_revision && left.incarnation_id == right.incarnation_id && left.session_id == right.session_id
}
pub(super) fn check_runtime(connection: &Connection, subject: &str, binding: &Binding) -> Result<(), St3Error> {
    let desired: Option<String> = connection.query_row("SELECT claim_id FROM desired WHERE subject=?1 AND kind='agent'", [subject], |row| row.get(0)).optional().map_err(internal)?;
    if desired.as_deref() != Some(&binding.desired_revision) {
        return Err(St3Error::new("stale-harness-control", "the runtime declaration changed"));
    }
    check_mailbox_incarnation(connection, &crate::mailbox::Fence { subject: subject.into(), incarnation: binding.incarnation_id.clone(), component: "delivery".into(), epoch: 0, token: String::new() })
}
pub(super) fn check_binding(connection: &Connection, subject: &str, binding: &Binding) -> Result<NativeState, St3Error> {
    check_runtime(connection, subject, binding)?;
    let current = state_tx(connection, subject)?.ok_or_else(|| St3Error::new("unsupported-harness-control", "this runtime has not reported a native control binding"))?;
    if &current.binding != binding {
        return Err(St3Error::new("stale-harness-control", "the native session or activity generation changed"));
    }
    Ok(current)
}
pub(super) fn reserve_dispatch_tx(connection: &Connection, subject: &str, operation: &str, kind: &str, binding: &Binding) -> Result<bool, St3Error> {
    Ok(connection.execute("INSERT OR IGNORE INTO local_harness_control_dispatch(subject,operation_id,kind,binding) VALUES(?1,?2,?3,?4)", params![subject, operation, kind, serde_json::to_string(binding).map_err(internal)?]).map_err(internal)? == 1)
}
pub(super) fn release_dispatch_tx(connection: &Connection, subject: &str, operation: &str) -> Result<(), St3Error> {
    connection.execute("DELETE FROM local_harness_control_dispatch WHERE subject=?1 AND operation_id=?2", params![subject, operation]).map_err(internal)?;
    Ok(())
}
fn pending_entry(queue: &Queue, id: &str) -> Result<usize, St3Error> {
    let index = queue.entries.iter().position(|entry| entry.id == id).ok_or_else(|| St3Error::new("missing-queue-entry", "queue entry does not exist"))?;
    if queue.entries[index].status != Outcome::Accepted {
        return Err(St3Error::new("already-dispatched", "only owner-pending input may be changed"));
    }
    Ok(index)
}
fn content(value: &str) -> Result<(), St3Error> {
    if value.trim().is_empty() || value.len() > MAX_CONTENT || value.contains('\0') {
        return Err(St3Error::new("invalid-queue-content", "input must be nonempty, without NUL, and at most 64 KiB"));
    }
    Ok(())
}
fn invalidate_pending(connection: &Connection, subject: &str, reason: &str) -> Result<(), St3Error> {
    let mut queue = queue_tx(connection, subject)?;
    let mut changed = false;
    for entry in &mut queue.entries {
        if matches!(entry.status, Outcome::Accepted | Outcome::Dispatched) {
            entry.status = if entry.status == Outcome::Dispatched { Outcome::Indeterminate } else { Outcome::Rejected };
            entry.reason = Some(reason.into());
            changed = true;
        }
    }
    if changed {
        queue.revision = queue.revision.checked_add(1).ok_or_else(|| St3Error::new("queue-revision-exhausted", "queue revision exhausted"))?;
        for entry in &queue.entries { save_receipt(connection, &receipt_for(entry, subject, queue.revision))?; }
        save_queue(connection, subject, &mut queue)?;
    }
    connection.execute("DELETE FROM local_harness_control_dispatch WHERE subject=?1 AND kind='input'", [subject]).map_err(internal)?;
    harness_model::invalidate_binding_tx(connection, subject, reason)?;
    Ok(())
}
fn reconcile_runtime(connection: &Connection, subject: &str) -> Result<(), St3Error> {
    let Some(mut state) = state_tx(connection, subject)? else { return Ok(()); };
    if let Err(error) = check_runtime(connection, subject, &state.binding) {
        if !matches!(error.code, "stale-harness-control" | "stale-mailbox-session") { return Err(error); }
        invalidate_pending(connection, subject, "native-runtime-ended-or-replaced")?;
        state.input_supported = false;
        state.models.available = false;
        state.reason = Some("native-runtime-ended-or-replaced".into());
        connection.execute("UPDATE local_harness_control_state SET state=?2 WHERE subject=?1", params![subject, serde_json::to_string(&state).map_err(internal)?]).map_err(internal)?;
    }
    Ok(())
}

impl Store {
    pub fn harness_control_queue(&self, subject: &str) -> Result<Queue, St3Error> {
        self.connection.batched(|tx| -> Result<Queue, St3Error> { reconcile_runtime(tx, subject)?; queue_tx(tx, subject) }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn harness_control_state(&self, subject: &str) -> Result<Option<NativeState>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<NativeState>, St3Error> {
            reconcile_runtime(tx, subject)?;
            state_tx(tx, subject)
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn harness_control_desired_revision(&self, fence: &crate::mailbox::Fence) -> Result<String, St3Error> {
        let connection = self.readers.get();
        check_mailbox_fence(&connection, fence)?;
        connection.query_row("SELECT claim_id FROM desired WHERE subject=?1 AND kind='agent'", [&fence.subject], |row| row.get(0)).map_err(internal)
    }
    pub fn harness_control_receipt(&self, operation: &str) -> Result<Option<Receipt>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<Receipt>, St3Error> {
            let subject: Option<String> = tx.query_row("SELECT subject FROM local_harness_control_operations WHERE id=?1", [operation], |row| row.get(0)).optional().map_err(internal)?;
            let Some(subject) = subject else { return Ok(None); };
            reconcile_runtime(tx, &subject)?;
            decode(tx.query_row("SELECT receipt FROM local_harness_control_operations WHERE id=?1", [operation], |row| row.get(0)).optional().map_err(internal)?)
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn close_harness_control(&self, fence: &crate::mailbox::Fence) -> Result<(), St3Error> {
        self.connection.batched(|tx| -> Result<(), St3Error> {
            check_mailbox_fence(tx, fence)?;
            invalidate_pending(tx, &fence.subject, "native-channel-closed")?;
            if let Some(mut state) = state_tx(tx, &fence.subject)? {
                state.input_supported = false;
                state.models.available = false;
                state.reason = Some("native-channel-closed".into());
                tx.execute("UPDATE local_harness_control_state SET state=?2 WHERE subject=?1", params![fence.subject, serde_json::to_string(&state).map_err(internal)?]).map_err(internal)?;
            }
            Ok(())
        }).map_err(|error| St3Error::new("internal", error))?
    }
    /// Authorization is established at the paired boundary; reservation and all runtime
    /// dependencies are checked again under the same writer transaction as the durable receipt.
    pub fn mutate_harness_queue(&self, request: &QueueRequest) -> Result<Receipt, St3Error> {
        if !request.actor.starts_with("person/") || request.actor.split('/').count() != 2 || request.actor == "person/" {
            return Err(St3Error::new("forbidden", "harness input requires a concrete person"));
        }
        if !(16..=256).contains(&request.idempotency_key.len()) {
            return Err(St3Error::new("invalid-idempotency-key", "idempotency key must contain 16 to 256 bytes"));
        }
        let operation = format!("operation/harness-{}", hex::encode(Sha256::digest(
            serde_json::to_vec(&(&request.actor, &request.idempotency_key)).map_err(internal)?
        )));
        // Fences may legitimately change on a retry; semantic input may not.
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&(request.subject.as_str(), request.actor.as_str(), &request.mutation)).map_err(internal)?));
        self.connection.batched(|tx| -> Result<Receipt, St3Error> {
            reconcile_runtime(tx, &request.subject)?;
            let prior: Option<(String, String)> = tx.query_row("SELECT digest,receipt FROM local_harness_control_operations WHERE id=?1", [&operation], |row| Ok((row.get(0)?, row.get(1)?))).optional().map_err(internal)?;
            if let Some((stored, receipt)) = prior {
                if stored != digest { return Err(St3Error::new("idempotency-conflict", "this operation key identifies different input")); }
                return serde_json::from_str(&receipt).map_err(internal);
            }
            if !check_binding(tx, &request.subject, &request.binding)?.input_supported {
                return Err(St3Error::new("unsupported-harness-control", "native input admission is unavailable"));
            }
            let mut queue = queue_tx(tx, &request.subject)?;
            if queue.revision != request.queue_revision { return Err(St3Error::new("stale-queue", "the owner queue changed")); }
            let entry_id;
            let status;
            match &request.mutation {
                QueueMutation::Enqueue { content: text, lane } => {
                    if *lane == Lane::Steer {
                        return Err(St3Error::new("native-pre-dequeue-api-unavailable", "native steering is unsupported without a public consumption fence"));
                    }
                    content(text)?;
                    if queue.entries.iter().filter(|entry| matches!(entry.status, Outcome::Accepted | Outcome::Dispatched)).count() >= MAX_PENDING {
                        return Err(St3Error::new("queue-full", "the owner queue contains 128 unsettled inputs"));
                    }
                    let id = format!("queue-entry/{}", operation.trim_start_matches("operation/"));
                    queue.entries.push(QueueEntry { id: id.clone(), actor: request.actor.clone(), content: text.clone(), lane: *lane, binding: request.binding.clone(), status: Outcome::Accepted, operation_id: operation.clone(), reason: None, result: None });
                    entry_id = id;
                    status = Outcome::Accepted;
                }
                QueueMutation::Move { entry_id: id, before_id } => {
                    let index = pending_entry(&queue, id)?;
                    if before_id.as_deref() == Some(id) { return Err(St3Error::new("invalid-queue-move", "an input cannot move before itself")); }
                    if let Some(before) = before_id { pending_entry(&queue, before)?; }
                    let entry = queue.entries.remove(index);
                    let destination = before_id.as_ref().and_then(|before| queue.entries.iter().position(|entry| &entry.id == before)).unwrap_or(queue.entries.len());
                    queue.entries.insert(destination, entry);
                    entry_id = id.clone(); status = Outcome::Applied;
                }
                QueueMutation::Cancel { entry_id: id } => {
                    let index = pending_entry(&queue, id)?;
                    queue.entries[index].status = Outcome::Cancelled;
                    entry_id = id.clone(); status = Outcome::Applied;
                }
                QueueMutation::Replace { entry_id: id, content: text } => {
                    content(text)?;
                    let index = pending_entry(&queue, id)?;
                    queue.entries[index].content.clone_from(text);
                    entry_id = id.clone(); status = Outcome::Applied;
                }
                QueueMutation::Promote { .. } => {
                    return Err(St3Error::new("native-pre-dequeue-api-unavailable", "native steering is unsupported without a public consumption fence"));
                }
            }
            queue.revision = queue.revision.checked_add(1).ok_or_else(|| St3Error::new("queue-revision-exhausted", "queue revision exhausted"))?;
            for entry in &queue.entries {
                save_receipt(tx, &receipt_for(entry, &request.subject, queue.revision))?;
            }
            let receipt = Receipt { operation_id: operation.clone(), subject: request.subject.clone(), entry_id: Some(entry_id), status, queue_revision: queue.revision, reason: None, result: None, binding: request.binding.clone() };
            tx.execute("INSERT INTO local_harness_control_operations(id,subject,digest,receipt) VALUES(?1,?2,?3,?4)", params![operation, request.subject, digest, serde_json::to_string(&receipt).map_err(internal)?]).map_err(internal)?;
            save_queue(tx, &request.subject, &mut queue)?;
            Ok(receipt)
        }).map_err(|error| St3Error::new("internal", error))?
    }
    /// The authenticated native driver supplies this baseline. A replacement never inherits
    /// permission to replay an uncertain operation from its predecessor.
    pub fn observe_harness_control(&self, state: &NativeState, fence: &crate::mailbox::Fence) -> Result<(), St3Error> {
        self.connection.batched(|tx| -> Result<(), St3Error> {
            check_mailbox_fence(tx, fence)?;
            if state.subject != fence.subject || state.binding.incarnation_id != fence.incarnation {
                return Err(St3Error::new("foreign-harness-control", "control state belongs to another runtime"));
            }
            check_runtime(tx, &state.subject, &state.binding)?;
            if state.binding.session_id.is_empty() {
                return Err(St3Error::new("invalid-harness-control", "native control must report session identity"));
            }
            let prior = state_tx(tx, &state.subject)?;
            if prior.as_ref().is_some_and(|prior| !same_session(&prior.binding, &state.binding)) {
                invalidate_pending(tx, &state.subject, "native-binding-replaced")?;
            }
            tx.execute("INSERT INTO local_harness_control_state(subject,state) VALUES(?1,?2) ON CONFLICT(subject) DO UPDATE SET state=excluded.state", params![state.subject, serde_json::to_string(state).map_err(internal)?]).map_err(internal)?;
            Ok(())
        }).map_err(|error| St3Error::new("internal", error))?
    }
    /// Reserve before returning bytes to the driver. A second poll cannot resend them.
    pub fn take_harness_input(&self, subject: &str, fence: &crate::mailbox::Fence) -> Result<Option<InputCommand>, St3Error> {
        self.connection.batched(|tx| -> Result<Option<InputCommand>, St3Error> {
            check_mailbox_fence(tx, fence)?;
            if subject != fence.subject { return Err(St3Error::new("foreign-harness-control", "input belongs to another seat")); }
            let Some(state) = state_tx(tx, subject)? else { return Ok(None); };
            check_binding(tx, subject, &state.binding)?;
            if !state.input_supported || !state.idle { return Ok(None); }
            let mut queue = queue_tx(tx, subject)?;
            // An unresolved input retains the native transition interlock and owns this lane.
            if queue.entries.iter().any(|entry| entry.status == Outcome::Dispatched) { return Ok(None); }
            let Some(index) = queue.entries.iter().position(|entry| entry.status == Outcome::Accepted && entry.lane == Lane::FollowUp) else { return Ok(None); };
            let entry = &mut queue.entries[index];
            if !same_session(&entry.binding, &state.binding) {
                return Err(St3Error::new("stale-harness-control", "pending input was accepted for another native session"));
            }
            if !reserve_dispatch_tx(tx, subject, &entry.operation_id, "input", &state.binding)? { return Ok(None); }
            entry.binding = state.binding;
            entry.status = Outcome::Dispatched;
            let command = InputCommand { operation_id: entry.operation_id.clone(), entry_id: entry.id.clone(), actor: entry.actor.clone(), content: entry.content.clone(), lane: entry.lane, binding: entry.binding.clone() };
            queue.revision += 1;
            save_receipt(tx, &receipt_for(&queue.entries[index], subject, queue.revision))?;
            save_queue(tx, subject, &mut queue)?;
            Ok(Some(command))
        }).map_err(|error| St3Error::new("internal", error))?
    }
    pub fn settle_harness_input(&self, receipt: &NativeReceipt, fence: &crate::mailbox::Fence) -> Result<Receipt, St3Error> {
        if !matches!(receipt.status, Outcome::Applied | Outcome::Rejected | Outcome::Indeterminate) {
            return Err(St3Error::new("invalid-native-receipt", "native settlement must be applied, rejected, or indeterminate"));
        }
        if receipt.status == Outcome::Applied && !matches!(&receipt.result, Some(st3_schema::harness_control::NativeResult::Input(result)) if matches!(result.native_event.as_str(), "message_start" | "message_end")) {
            return Err(St3Error::new("invalid-native-receipt", "applied input requires an exact native input event"));
        }
        self.connection.batched(|tx| -> Result<Receipt, St3Error> {
            check_mailbox_fence(tx, fence)?;
            if receipt.subject != fence.subject || receipt.binding.incarnation_id != fence.incarnation {
                return Err(St3Error::new("foreign-harness-control", "receipt belongs to another runtime"));
            }
            let prior: Option<String> = tx.query_row("SELECT receipt FROM local_harness_control_operations WHERE id=?1 AND subject=?2", params![receipt.operation_id, receipt.subject], |row| row.get(0)).optional().map_err(internal)?;
            if let Some(prior) = prior {
                let prior: Receipt = serde_json::from_str(&prior).map_err(internal)?;
                if !matches!(prior.status, Outcome::Accepted | Outcome::Dispatched) {
                    if prior.binding == receipt.binding && prior.status == receipt.status && prior.reason == receipt.reason && prior.result == receipt.result { return Ok(prior); }
                    return Err(St3Error::new("already-settled", "native operation is already settled"));
                }
            }
            check_runtime(tx, &receipt.subject, &receipt.binding)?;
            let mut queue = queue_tx(tx, &receipt.subject)?;
            let entry = queue.entries.iter_mut().find(|entry| entry.operation_id == receipt.operation_id).ok_or_else(|| St3Error::new("missing-harness-operation", "native operation does not exist"))?;
            if entry.binding != receipt.binding { return Err(St3Error::new("stale-harness-control", "native receipt does not match the reserved input")); }
            if entry.status != Outcome::Dispatched {
                if entry.status == receipt.status && entry.reason == receipt.reason && entry.result == receipt.result {
                    let stored: String = tx.query_row("SELECT receipt FROM local_harness_control_operations WHERE id=?1", [&receipt.operation_id], |row| row.get(0)).map_err(internal)?;
                    return serde_json::from_str(&stored).map_err(internal);
                }
                return Err(St3Error::new("already-settled", "native operation is already settled"));
            }
            entry.status = receipt.status;
            entry.reason.clone_from(&receipt.reason);
            entry.result.clone_from(&receipt.result);
            queue.revision += 1;
            let result = receipt_for(entry, &receipt.subject, queue.revision);
            save_receipt(tx, &result)?;
            release_dispatch_tx(tx, &receipt.subject, &receipt.operation_id)?;
            save_queue(tx, &receipt.subject, &mut queue)?;
            Ok(result)
        }).map_err(|error| St3Error::new("internal", error))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st3_schema::harness_control::{Approval, InputResult, Models, NativeResult};
    const SUBJECT: &str = "agent/control-smoke";
    fn baseline(store: &Store) -> (NativeState, crate::mailbox::Fence) {
        let intent = crate::graph::parse_intent("version 2\nagent \"control-smoke\" { workspace \".\"; harness \"omp\" { model \"control-smoke/native-smoke\"; } }", "queue-owner").unwrap();
        store.apply_as(&intent, &BTreeMap::new(), "control-test-declaration", Some("person/operator")).unwrap();
        store.append_claim(&ClaimInput { subject: SUBJECT.into(), kind: "runtime.observed".into(), actor: Some(SUBJECT.into()), fields: BTreeMap::from([("status".into(), json!("running")), ("incarnation_id".into(), json!("incarnation-1")), ("runtime_id".into(), json!("native-runtime"))]), evidence: Vec::new(), expected_subject: None, idempotency_key: None }).unwrap();
        let fence = store.bind_mailbox(&crate::mailbox::Fence::new(SUBJECT, "incarnation-1", "delivery")).unwrap();
        let state = NativeState { subject: SUBJECT.into(), binding: Binding { desired_revision: store.harness_control_desired_revision(&fence).unwrap(), incarnation_id: "incarnation-1".into(), session_id: "session-1".into(), turn_id: None }, idle: false, input_supported: true, steer: Default::default(), models: Models { choices: Vec::new(), selected: None, atomic_model_effort: false, revision: "models-1".into(), available: false, complete: true, source: "native-extension-model-registry".into() }, approval: Approval { supported: false, reason: "native-live-approval-api-unavailable".into() }, reason: None };
        store.observe_harness_control(&state, &fence).unwrap();
        (state, fence)
    }
    fn request(store: &Store, state: &NativeState, key: &str, mutation: QueueMutation) -> QueueRequest {
        QueueRequest { subject: SUBJECT.into(), actor: "person/operator".into(), idempotency_key: format!("smoke-operation-{key}"), binding: state.binding.clone(), queue_revision: store.harness_control_queue(SUBJECT).unwrap().revision, mutation }
    }
    fn enqueue(store: &Store, state: &NativeState, key: &str) -> Receipt {
        store.mutate_harness_queue(&request(store, state, key, QueueMutation::Enqueue { content: "identical text".into(), lane: Lane::FollowUp })).unwrap()
    }
    #[test]
    fn identical_inputs_are_edited_by_identity_and_only_pending_inputs_can_change() {
        let store = Store::open_memory("queue-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        let a = enqueue(&store, &state, "a");
        let b = enqueue(&store, &state, "b");
        let c = enqueue(&store, &state, "c");
        let aid = a.entry_id.unwrap(); let bid = b.entry_id.unwrap(); let cid = c.entry_id.unwrap();
        store.mutate_harness_queue(&request(&store, &state, "move", QueueMutation::Move { entry_id: cid.clone(), before_id: Some(aid.clone()) })).unwrap();
        let queue = store.harness_control_queue(SUBJECT).unwrap();
        assert_eq!(queue.entries.iter().map(|entry| &entry.id).collect::<Vec<_>>(), vec![&cid, &aid, &bid]);
        store.mutate_harness_queue(&request(&store, &state, "replace", QueueMutation::Replace { entry_id: bid.clone(), content: "replacement".into() })).unwrap();
        store.mutate_harness_queue(&request(&store, &state, "cancel", QueueMutation::Cancel { entry_id: aid.clone() })).unwrap();
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none(), "follow-up stays owner-held while native is busy");
        state.idle = true;
        store.observe_harness_control(&state, &fence).unwrap();
        let dispatched = store.take_harness_input(SUBJECT, &fence).unwrap().unwrap();
        assert_eq!((dispatched.entry_id.as_str(), dispatched.content.as_str(), dispatched.lane), (cid.as_str(), "identical text", Lane::FollowUp));
        let error = store.mutate_harness_queue(&request(&store, &state, "late-cancel", QueueMutation::Cancel { entry_id: cid.clone() })).unwrap_err();
        assert_eq!(error.code, "already-dispatched");
        let queue = store.harness_control_queue(SUBJECT).unwrap();
        assert_eq!(store.harness_control_receipt(&a.operation_id).unwrap().unwrap().status, Outcome::Cancelled);
        assert_eq!(queue.entries.iter().find(|entry| entry.id == cid).unwrap().content, "identical text");
    }
    #[test]
    fn reservation_survives_reopen_and_receipts_freeze_native_proof_without_resending() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owner.sqlite");
        let store = Store::open(&path, "queue-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        state.idle = true;
        store.observe_harness_control(&state, &fence).unwrap();
        let original = request(&store, &state, "durable", QueueMutation::Enqueue { content: "durable".into(), lane: Lane::FollowUp });
        let accepted = store.mutate_harness_queue(&original).unwrap();
        let command = store.take_harness_input(SUBJECT, &fence).unwrap().unwrap();
        drop(store);
        let store = Store::open(&path, "queue-owner").unwrap();
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none(), "an uncertain native handoff is never resent");
        assert_eq!(store.mutate_harness_queue(&original).unwrap().status, Outcome::Dispatched);
        let proof = NativeReceipt { subject: SUBJECT.into(), binding: command.binding, operation_id: accepted.operation_id, status: Outcome::Applied, reason: None, result: Some(NativeResult::Input(InputResult { native_event: "message_start".into(), turn_id: Some("turn-1".into()) })) };
        let settled = store.settle_harness_input(&proof, &fence).unwrap();
        assert_eq!(settled.status, Outcome::Applied);
        assert_eq!(settled.result, proof.result);
        assert_eq!(store.settle_harness_input(&proof, &fence).unwrap(), settled);
        assert_eq!(store.mutate_harness_queue(&original).unwrap(), settled, "response loss recovers the current frozen outcome");
        let mut changed = original; changed.mutation = QueueMutation::Enqueue { content: "different".into(), lane: Lane::FollowUp };
        assert_eq!(store.mutate_harness_queue(&changed).unwrap_err().code, "idempotency-conflict");
        let mut forged = proof; forged.result = Some(NativeResult::Input(InputResult { native_event: "message_end".into(), turn_id: Some("different".into()) }));
        assert_eq!(store.settle_harness_input(&forged, &fence).unwrap_err().code, "already-settled");
    }
    #[test]
    fn operation_identity_keeps_actor_and_idempotency_key_boundaries() {
        let store = Store::open_memory("queue-owner").unwrap();
        let (state, _) = baseline(&store);
        let mut first = request(&store, &state, "identity", QueueMutation::Enqueue {
            content: "first person".into(), lane: Lane::FollowUp,
        });
        first.actor = "person/ada:key".into();
        first.idempotency_key = "0123456789abcdef".into();
        let accepted_first = store.mutate_harness_queue(&first).unwrap();
        let mut second = request(&store, &state, "identity", QueueMutation::Enqueue {
            content: "second person".into(), lane: Lane::FollowUp,
        });
        second.actor = "person/ada".into();
        second.idempotency_key = "key:0123456789abcdef".into();
        let accepted_second = store.mutate_harness_queue(&second).unwrap();
        assert_ne!(accepted_first.operation_id, accepted_second.operation_id);
        let recovered_first = store.mutate_harness_queue(&first).unwrap();
        assert_eq!(recovered_first.operation_id, accepted_first.operation_id);
        assert_eq!(recovered_first.entry_id, accepted_first.entry_id);
        assert_eq!(recovered_first, store.harness_control_receipt(&accepted_first.operation_id).unwrap().unwrap());
        let recovered_second = store.mutate_harness_queue(&second).unwrap();
        assert_eq!(recovered_second.operation_id, accepted_second.operation_id);
        assert_eq!(recovered_second.entry_id, accepted_second.entry_id);
        assert_eq!(recovered_second, store.harness_control_receipt(&accepted_second.operation_id).unwrap().unwrap());
        let queue = store.harness_control_queue(SUBJECT).unwrap();
        assert_eq!((queue.entries[0].actor.as_str(), queue.entries[0].content.as_str()), (first.actor.as_str(), "first person"));
        assert_eq!((queue.entries[1].actor.as_str(), queue.entries[1].content.as_str()), (second.actor.as_str(), "second person"));
    }

    #[test]
    fn stale_dependencies_reject_and_session_replacement_never_inherits_uncertain_input() {
        let store = Store::open_memory("queue-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        for field in ["desired", "incarnation", "session", "turn", "revision"] {
            let mut stale = request(&store, &state, field, QueueMutation::Enqueue { content: "stale".into(), lane: Lane::FollowUp });
            match field { "desired" => stale.binding.desired_revision = "old".into(), "incarnation" => stale.binding.incarnation_id = "old".into(), "session" => stale.binding.session_id = "old".into(), "turn" => stale.binding.turn_id = Some("old".into()), _ => stale.queue_revision += 1 }
            let error = store.mutate_harness_queue(&stale).unwrap_err();
            assert!(matches!(error.code, "stale-harness-control" | "stale-mailbox-session" | "stale-queue"), "{field}: {error}");
        }
        let mut unauthorized = request(&store, &state, "actor", QueueMutation::Enqueue { content: "actor".into(), lane: Lane::FollowUp });
        unauthorized.actor = "person/".into();
        assert_eq!(store.mutate_harness_queue(&unauthorized).unwrap_err().code, "forbidden");
        let pending = enqueue(&store, &state, "pending");
        let active = enqueue(&store, &state, "active");
        store.mutate_harness_queue(&request(&store, &state, "active-first", QueueMutation::Move { entry_id: active.entry_id.clone().unwrap(), before_id: pending.entry_id.clone() })).unwrap();
        state.idle = true;
        store.observe_harness_control(&state, &fence).unwrap();
        store.take_harness_input(SUBJECT, &fence).unwrap().unwrap();
        state.binding.session_id = "session-2".into();
        store.observe_harness_control(&state, &fence).unwrap();
        assert_eq!(store.harness_control_receipt(&pending.operation_id).unwrap().unwrap().status, Outcome::Rejected);
        assert_eq!(store.harness_control_receipt(&active.operation_id).unwrap().unwrap().status, Outcome::Indeterminate);
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none());
    }
    #[test]
    fn unsupported_steer_does_not_change_owner_queue_and_follow_up_waits_for_idle() {
        let store = Store::open_memory("queue-owner").unwrap();
        let (mut state, fence) = baseline(&store);
        state.binding.turn_id = Some("turn-a".into());
        store.observe_harness_control(&state, &fence).unwrap();
        let follow_up = enqueue(&store, &state, "follow-up-turn");
        let before = store.harness_control_queue(SUBJECT).unwrap();
        for mutation in [
            QueueMutation::Enqueue { content: "unsupported".into(), lane: Lane::Steer },
            QueueMutation::Promote { entry_id: follow_up.entry_id.clone().unwrap() },
        ] {
            let error = store.mutate_harness_queue(&request(&store, &state, "unsupported-steer", mutation)).unwrap_err();
            assert_eq!(error.code, "native-pre-dequeue-api-unavailable");
            assert_eq!(store.harness_control_queue(SUBJECT).unwrap(), before);
        }
        state.binding.turn_id = Some("turn-b".into());
        store.observe_harness_control(&state, &fence).unwrap();
        assert!(store.take_harness_input(SUBJECT, &fence).unwrap().is_none());
        state.idle = true;
        store.observe_harness_control(&state, &fence).unwrap();
        let command = store.take_harness_input(SUBJECT, &fence).unwrap().unwrap();
        assert_eq!(command.operation_id, follow_up.operation_id);
        assert_eq!(command.binding.turn_id.as_deref(), Some("turn-b"));
        assert_eq!(command.lane, Lane::FollowUp);
    }
}
