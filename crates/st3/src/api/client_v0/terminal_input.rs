//! Ordered terminal input on the collection socket. An input binds to a terminal follow the
//! socket already holds, so it inherits that follow's viewer lease and fenced incarnation;
//! `terminal.control` is checked once, when it opens. Batches carry a sequence from
//! `next_seq`: a repeat is acknowledged without a write, a gap closes the input. Before writing
//! a new batch it rechecks the device pairing, viewer record and incarnation; any failure
//! closes the input. Nothing is ever resent.

use super::*;

/// The most bytes one input batch writes.
const TERMINAL_INPUT_MAX_BATCH_BYTES: usize = 16 * 1024;

/// What a batch to a followed terminal is checked against: the incarnation the follow fenced,
/// and the viewer record's head held by that follow.
#[derive(Clone)]
pub(super) struct InputTarget {
    subject: String,
    incarnation: String,
    attachment: String,
    consumed: String,
}

impl TerminalFollow {
    /// Input reaches only a terminal this host owns; another host's terminal has no target.
    pub(super) fn input_target(&self) -> Option<InputTarget> {
        let Self::Local {
            id,
            incarnation,
            viewer,
        } = self
        else {
            return None;
        };
        Some(InputTarget {
            subject: terminal_subject(id),
            incarnation: incarnation.clone(),
            attachment: viewer.subject.clone(),
            consumed: viewer.consumed_id.clone(),
        })
    }
}

/// One batch: text is written as its UTF-8 bytes, `bytes_b64` as the bytes it encodes.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalInputData {
    text: Option<String>,
    bytes_b64: Option<String>,
}

impl TerminalInputData {
    fn bytes(&self) -> Result<Vec<u8>, String> {
        let bytes = match (&self.text, &self.bytes_b64) {
            (Some(text), None) => text.as_bytes().to_vec(),
            (None, Some(encoded)) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("input bytes are not valid base64: {error}"))?,
            _ => return Err("input data holds exactly one of `text` or `bytes_b64`".into()),
        };
        if bytes.is_empty() || bytes.len() > TERMINAL_INPUT_MAX_BATCH_BYTES {
            return Err(format!(
                "an input batch holds 1 through {TERMINAL_INPUT_MAX_BATCH_BYTES} bytes"
            ));
        }
        if bytes.contains(&0) {
            return Err("an input batch cannot contain a NUL byte".into());
        }
        Ok(bytes)
    }
}

use st3_schema::input_sessions::{
    InputSessionCloseReason as CloseReason, InputSessionEvent, InputSessionRecord,
    MAX_AUDIT_INTEGER,
};

struct OpenInput {
    follow: String,
    next_seq: u64,
    audit: InputSessionRecord,
    last_checkpoint: tokio::time::Instant,
    checkpoint_bytes: u64,
}

/// Socket-owned authority; durable facts belong to the terminal owner's graph.
#[derive(Default)]
pub(super) struct TerminalInputs {
    targets: BTreeMap<String, Option<InputTarget>>,
    open: BTreeMap<String, OpenInput>,
}

struct InputFailure {
    reason: CloseReason,
    message: String,
    uncertain_handoff: bool,
}

impl InputFailure {
    fn rejected(message: impl ToString) -> Self {
        Self {
            reason: CloseReason::Rejected,
            message: message.to_string(),
            uncertain_handoff: false,
        }
    }
}

fn wire_reason(reason: CloseReason) -> &'static str {
    match reason {
        CloseReason::Gap => "gap",
        CloseReason::Detached
        | CloseReason::ClientClose
        | CloseReason::Replaced
        | CloseReason::SocketDisconnected => "detached",
        CloseReason::IncarnationChanged | CloseReason::OwnerRestarted => "incarnation-changed",
        CloseReason::Revoked => "revoked",
        CloseReason::Rejected | CloseReason::AuditUnavailable => "rejected",
    }
}

fn closed(id: &str, reason: CloseReason, message: impl std::fmt::Display) -> Value {
    json!({"kind":"input-closed", "id":id, "reason":wire_reason(reason), "message":message.to_string()})
}

async fn persist(state: &AppState, record: &InputSessionRecord) -> Result<(), ApiError> {
    let (store, record) = (state.store.clone(), record.clone());
    tokio::task::spawn_blocking(move || store.append_input_session(&record))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    signal_visible_change(state);
    Ok(())
}

impl OpenInput {
    fn snapshot(
        &self,
        event: InputSessionEvent,
        reason: Option<CloseReason>,
    ) -> Result<InputSessionRecord, ApiError> {
        let mut record = self.audit.clone();
        record.ordinal = record
            .ordinal
            .checked_add(1)
            .filter(|ordinal| *ordinal <= MAX_AUDIT_INTEGER)
            .ok_or_else(|| ApiError::internal("the input audit ordinal is exhausted"))?;
        record.event = event;
        record.reason = reason;
        record.observed_at_unix_ms = (client_now_ms() as u64).max(record.observed_at_unix_ms);
        Ok(record)
    }

    async fn finish(self, state: &AppState, reason: CloseReason) -> Result<(), ApiError> {
        let record = self.snapshot(InputSessionEvent::Closed, Some(reason))?;
        persist(state, &record).await
    }
}

impl TerminalInputs {
    pub(super) fn follow(&mut self, follow: &str, target: Option<InputTarget>) {
        self.targets.insert(follow.to_owned(), target);
    }

    pub(super) fn has_open(&self) -> bool {
        !self.open.is_empty()
    }

    async fn end_input(
        &mut self,
        state: &AppState,
        id: &str,
        reason: CloseReason,
    ) -> Result<bool, ApiError> {
        let Some(input) = self.open.remove(id) else {
            return Ok(false);
        };
        if let Err(error) = input.finish(state, reason).await {
            tracing::warn!(
                input = id,
                code = error.code,
                "could not finalize the terminal input audit"
            );
            return Err(error);
        }
        Ok(true)
    }

    /// All normal socket exits await finalization. Cancellation/process loss deliberately
    /// leaves the last durable lower bound; startup recovery labels that suffix interrupted.
    pub(super) async fn finish_all(&mut self, state: &AppState) {
        for (id, input) in std::mem::take(&mut self.open) {
            if let Err(error) = input.finish(state, CloseReason::SocketDisconnected).await {
                tracing::warn!(
                    input = id,
                    code = error.code,
                    "could not finalize the terminal input audit"
                );
            }
        }
    }

    pub(super) async fn end_follow(
        &mut self,
        state: &AppState,
        follow: &str,
        error_code: Option<&str>,
    ) -> Option<Value> {
        self.targets.remove(follow)?;
        let id = self
            .open
            .iter()
            .find(|(_, input)| input.follow == follow)
            .map(|(id, _)| id.clone())?;
        let reason = if matches!(error_code, Some("stale-fence" | "terminal-ended")) {
            CloseReason::IncarnationChanged
        } else {
            CloseReason::Detached
        };
        match self.end_input(state, &id, reason).await {
            Ok(_) => Some(closed(&id, reason, "the terminal follow ended")),
            Err(_) => Some(closed(
                &id,
                CloseReason::AuditUnavailable,
                "the input audit could not be finalized",
            )),
        }
    }

    /// Revoke idle authority on graph changes/clock ticks and persist sparse cumulative
    /// progress. A rate cap is not a promised maximum uncheckpointed time/byte interval.
    pub(super) async fn maintain(
        &mut self,
        state: &AppState,
        session: &ClientSession,
    ) -> Vec<Value> {
        if self.open.is_empty() {
            return Vec::new();
        }
        let checks = self
            .open
            .iter()
            .filter_map(|(id, input)| {
                self.targets
                    .get(&input.follow)
                    .cloned()
                    .flatten()
                    .map(|target| (id.clone(), target))
            })
            .collect::<Vec<_>>();
        let (checked_state, checked_session) = (state.clone(), session.clone());
        let failures = tokio::task::spawn_blocking(move || {
            checks
                .into_iter()
                .filter_map(|(id, target)| {
                    check_target(&checked_state, &checked_session, &target)
                        .err()
                        .map(|error| (id, error))
                })
                .collect::<Vec<_>>()
        })
        .await;
        let mut frames = Vec::new();
        match failures {
            Ok(failures) => {
                for (id, failure) in failures {
                    let result = self.end_input(state, &id, failure.reason).await;
                    frames.push(match result {
                        Ok(_) => closed(&id, failure.reason, failure.message),
                        Err(_) => closed(
                            &id,
                            CloseReason::AuditUnavailable,
                            "the input audit could not be finalized",
                        ),
                    });
                }
            }
            Err(_) => {
                for id in self.open.keys().cloned().collect::<Vec<_>>() {
                    let _ = self
                        .end_input(state, &id, CloseReason::AuditUnavailable)
                        .await;
                    frames.push(closed(
                        &id,
                        CloseReason::AuditUnavailable,
                        "the input authority check failed",
                    ));
                }
            }
        }
        let dirty = self
            .open
            .iter()
            .filter(|(_, input)| {
                input.audit.successful_send_bytes != input.checkpoint_bytes
                    && input.last_checkpoint.elapsed() >= Duration::from_secs(1)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in dirty {
            let Some(input) = self.open.get(&id) else {
                continue;
            };
            // Reserve one durable ordinal for a final close or restart interruption.
            if input.audit.ordinal >= MAX_AUDIT_INTEGER - 1 {
                let _ = self.end_input(state, &id, CloseReason::Rejected).await;
                frames.push(closed(
                    &id,
                    CloseReason::Rejected,
                    "the input audit ordinal is exhausted",
                ));
                continue;
            }
            let result = match input.snapshot(InputSessionEvent::Checkpoint, None) {
                Ok(record) => persist(state, &record).await.map(|()| record),
                Err(error) => Err(error),
            };
            match result {
                Ok(record) => {
                    if let Some(input) = self.open.get_mut(&id) {
                        input.audit.ordinal = record.ordinal;
                        input.audit.observed_at_unix_ms = record.observed_at_unix_ms;
                        input.checkpoint_bytes = record.successful_send_bytes;
                        input.last_checkpoint = tokio::time::Instant::now();
                    }
                }
                Err(_) => {
                    let _ = self
                        .end_input(state, &id, CloseReason::AuditUnavailable)
                        .await;
                    frames.push(closed(
                        &id,
                        CloseReason::AuditUnavailable,
                        "the input audit could not checkpoint progress",
                    ));
                }
            }
        }
        frames
    }

    pub(super) async fn command(
        &mut self,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
    ) -> Option<Value> {
        let id = request.id.as_str();
        match request.kind.as_str() {
            "input-open" => {
                // Replacement also closes a previously held ID when the new open is invalid.
                if self
                    .end_input(state, id, CloseReason::Replaced)
                    .await
                    .is_err()
                {
                    return Some(closed(
                        id,
                        CloseReason::AuditUnavailable,
                        "the previous input audit could not be finalized",
                    ));
                }
                if id.is_empty() || id.len() > 128 {
                    return Some(closed(id, CloseReason::Rejected, "invalid input ID"));
                }
                if let Err(error) = require_scope(session, "terminal.control") {
                    return Some(closed(id, CloseReason::Rejected, error.message));
                }
                let follow = request.follow.as_deref().unwrap_or_default();
                let target = match self.targets.get(follow) {
                    None => {
                        return Some(closed(
                            id,
                            CloseReason::Rejected,
                            "the input names no held terminal follow",
                        ));
                    }
                    Some(None) => {
                        return Some(closed(
                            id,
                            CloseReason::Rejected,
                            "input reaches only a terminal this host owns",
                        ));
                    }
                    Some(Some(target)) => target.clone(),
                };
                if self.open.values().any(|input| input.follow == follow) {
                    return Some(closed(
                        id,
                        CloseReason::Rejected,
                        "the terminal follow already has an open input",
                    ));
                }
                let (checked_state, checked_session, checked_target) =
                    (state.clone(), session.clone(), target.clone());
                let checked = tokio::task::spawn_blocking(move || {
                    check_target(&checked_state, &checked_session, &checked_target)
                })
                .await;
                match checked {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => return Some(closed(id, error.reason, error.message)),
                    Err(_) => {
                        return Some(closed(
                            id,
                            CloseReason::Rejected,
                            "the terminal authority check failed",
                        ));
                    }
                }
                let now = client_now_ms() as u64;
                let record = InputSessionRecord {
                    version: 1,
                    ordinal: 0,
                    event: InputSessionEvent::Opened,
                    session_id: uuid::Uuid::now_v7().to_string(),
                    owner: state.node.clone(),
                    owner_epoch: input_session_epoch().to_owned(),
                    terminal: target.subject,
                    incarnation: target.incarnation,
                    attachment: target.attachment,
                    attachment_claim: target.consumed,
                    device_id: session
                        .pairing
                        .as_ref()
                        .and_then(|pairing| pairing.device_id.clone()),
                    device_actor: session.actor.clone(),
                    authority_actor: session.authority_actor.clone(),
                    person: session
                        .authority_actor
                        .starts_with("person/")
                        .then(|| session.authority_actor.clone()),
                    pairing_claim: session
                        .pairing
                        .as_ref()
                        .map(|pairing| pairing.completed_claim.clone()),
                    opened_at_unix_ms: now,
                    observed_at_unix_ms: now,
                    successful_send_bytes: 0,
                    successful_batches: 0,
                    uncertain_handoff: false,
                    reason: None,
                };
                if persist(state, &record).await.is_err() {
                    return Some(closed(
                        id,
                        CloseReason::AuditUnavailable,
                        "the durable input audit could not be opened",
                    ));
                }
                self.open.insert(
                    id.to_owned(),
                    OpenInput {
                        follow: follow.to_owned(),
                        next_seq: 0,
                        audit: record,
                        last_checkpoint: tokio::time::Instant::now(),
                        checkpoint_bytes: 0,
                    },
                );
                Some(json!({"kind":"input-opened", "id":id, "follow":follow, "next_seq":0}))
            }
            "input-close" => match self.end_input(state, id, CloseReason::ClientClose).await {
                Ok(_) => None,
                Err(_) => Some(closed(
                    id,
                    CloseReason::AuditUnavailable,
                    "the input audit could not be finalized",
                )),
            },
            "input" => {
                let input = self.open.get(id)?;
                let Some(seq) = request.seq else {
                    let _ = self.end_input(state, id, CloseReason::Rejected).await;
                    return Some(closed(
                        id,
                        CloseReason::Rejected,
                        "an input batch carries `seq`",
                    ));
                };
                if seq < input.next_seq {
                    return Some(json!({"kind":"input-ack", "id":id, "seq":seq}));
                }
                if seq > input.next_seq {
                    let expected = input.next_seq;
                    let _ = self.end_input(state, id, CloseReason::Gap).await;
                    return Some(closed(
                        id,
                        CloseReason::Gap,
                        format!("expected seq {expected}, got {seq}"),
                    ));
                }
                let bytes = match request
                    .data
                    .as_ref()
                    .ok_or_else(|| "an input batch carries `data`".to_owned())
                    .and_then(TerminalInputData::bytes)
                {
                    Ok(bytes) => bytes,
                    Err(message) => {
                        let _ = self.end_input(state, id, CloseReason::Rejected).await;
                        return Some(closed(id, CloseReason::Rejected, message));
                    }
                };
                let byte_count = bytes.len() as u64;
                if input.next_seq.checked_add(1).is_none()
                    || input.audit.ordinal >= MAX_AUDIT_INTEGER
                    || input
                        .audit
                        .successful_send_bytes
                        .checked_add(byte_count)
                        .is_none_or(|count| count > MAX_AUDIT_INTEGER)
                    || input
                        .audit
                        .successful_batches
                        .checked_add(1)
                        .is_none_or(|count| count > MAX_AUDIT_INTEGER)
                {
                    let _ = self.end_input(state, id, CloseReason::Rejected).await;
                    return Some(closed(
                        id,
                        CloseReason::Rejected,
                        "the input counters are exhausted",
                    ));
                }
                let target = self.targets.get(&input.follow).cloned().flatten()?;
                let (checked_state, checked_session) = (state.clone(), session.clone());
                let written = tokio::task::spawn_blocking(move || {
                    write_batch(&checked_state, &checked_session, &target, &bytes)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(InputFailure {
                        reason: CloseReason::Rejected,
                        message: "the terminal send outcome is unknown".into(),
                        uncertain_handoff: true,
                    })
                });
                match written {
                    Ok(()) => {
                        let input = self.open.get_mut(id)?;
                        input.next_seq += 1;
                        input.audit.successful_send_bytes += byte_count;
                        input.audit.successful_batches += 1;
                        Some(json!({"kind":"input-ack", "id":id, "seq":seq}))
                    }
                    Err(error) => {
                        self.open.get_mut(id)?.audit.uncertain_handoff |= error.uncertain_handoff;
                        match self.end_input(state, id, error.reason).await {
                            Ok(_) => Some(closed(id, error.reason, error.message)),
                            Err(_) => Some(closed(
                                id,
                                CloseReason::AuditUnavailable,
                                "the input closed, but its durable outcome is incomplete",
                            )),
                        }
                    }
                }
            }
            _ => Some(closed(id, CloseReason::Rejected, "unknown input command")),
        }
    }
}

/// Preflight failure proves no dispatch; a send error can follow a partial Unix-socket write.
fn check_target(
    state: &AppState,
    session: &ClientSession,
    target: &InputTarget,
) -> Result<LiveSession, InputFailure> {
    if let Some(pairing) = &session.pairing
        && pairing_withdrawn(state, pairing)
            .map_err(|error| InputFailure::rejected(error.message))?
    {
        return Err(InputFailure {
            reason: CloseReason::Revoked,
            message: "the client credential was revoked or expired".into(),
            uncertain_handoff: false,
        });
    }
    let head = state
        .store
        .claims_for(&target.attachment, None)
        .map_err(InputFailure::rejected)?;
    if head.last().map(|claim| claim.id.as_str()) != Some(target.consumed.as_str()) {
        return Err(InputFailure {
            reason: CloseReason::Detached,
            message: "the terminal viewer was detached".into(),
            uncertain_handoff: false,
        });
    }
    terminal_live_session(state, &target.subject, Some(&target.incarnation)).map_err(|error| {
        InputFailure {
            reason: match error.code.as_str() {
                "stale-fence" | "not-found" => CloseReason::IncarnationChanged,
                _ => CloseReason::Rejected,
            },
            message: error.message,
            uncertain_handoff: false,
        }
    })
}

fn write_batch(
    state: &AppState,
    session: &ClientSession,
    target: &InputTarget,
    bytes: &[u8],
) -> Result<(), InputFailure> {
    let live = check_target(state, session, target)?;
    let runtime = daemon_pty(state).map_err(InputFailure::rejected)?;
    runtime
        .send_raw_if(&live.runtime_id, bytes, Some(&target.incarnation))
        .map_err(|error| InputFailure {
            reason: CloseReason::Rejected,
            uncertain_handoff: matches!(error, st_runtime::PtySendError::PossiblyDispatched(_)),
            message: error.to_string(),
        })
}
