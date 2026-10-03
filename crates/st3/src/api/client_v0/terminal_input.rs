//! Ordered terminal input on the collection socket. An input binds to a terminal follow the
//! socket already holds, so it inherits that follow's consumed viewer and fenced incarnation;
//! `terminal.control` is checked once, when it opens. Batches carry a sequence from
//! `next_seq`: a repeat is acknowledged without a write, a gap closes the input. Every batch
//! rechecks the device pairing, the viewer record and the incarnation first, and any failure
//! closes the input. Nothing is ever resent.

use super::*;

/// The most bytes one input batch writes.
const TERMINAL_INPUT_MAX_BATCH_BYTES: usize = 16 * 1024;

/// What a batch to a followed terminal is checked against: the incarnation the follow fenced,
/// and the viewer record it consumed.
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
        Ok(bytes)
    }
}

struct OpenInput {
    follow: String,
    next_seq: u64,
}

/// A socket's terminal follows, as input targets, and the inputs open on them.
#[derive(Default)]
pub(super) struct TerminalInputs {
    /// Every held terminal follow; `None` for one input cannot reach.
    targets: BTreeMap<String, Option<InputTarget>>,
    open: BTreeMap<String, OpenInput>,
}

fn closed(id: &str, reason: &str, message: impl std::fmt::Display) -> Value {
    json!({"kind":"input-closed", "id":id, "reason":reason, "message":message.to_string()})
}

impl TerminalInputs {
    pub(super) fn follow(&mut self, follow: &str, target: Option<InputTarget>) {
        self.targets.insert(follow.to_owned(), target);
    }

    /// A follow ended, so the input bound to it closes. A follow that ended on its incarnation
    /// fence or on process exit closes it with `incarnation-changed`; any other end, including
    /// unsubscribe and replacement, with `detached`.
    pub(super) fn end_follow(&mut self, follow: &str, error_code: Option<&str>) -> Option<Value> {
        self.targets.remove(follow)?;
        let id = self
            .open
            .iter()
            .find(|(_, input)| input.follow == follow)
            .map(|(id, _)| id.clone())?;
        self.open.remove(&id);
        let reason = if matches!(error_code, Some("stale-fence" | "terminal-ended")) {
            "incarnation-changed"
        } else {
            "detached"
        };
        Some(closed(&id, reason, "the terminal follow ended"))
    }

    /// Handle one `input-open`, `input` or `input-close` command; returns the frame to send.
    pub(super) async fn command(
        &mut self,
        state: &AppState,
        session: &ClientSession,
        request: &CollectionSubscribe,
    ) -> Option<Value> {
        let id = request.id.as_str();
        match request.kind.as_str() {
            "input-open" => {
                // A held input ID is replaced, as a held subscription ID is.
                self.open.remove(id);
                if id.is_empty() || id.len() > 128 {
                    return Some(closed(id, "rejected", "invalid input ID"));
                }
                if let Err(error) = require_scope(session, "terminal.control") {
                    return Some(closed(id, "rejected", error.message));
                }
                let follow = request.follow.as_deref().unwrap_or_default();
                match self.targets.get(follow) {
                    None => {
                        return Some(closed(id, "rejected", "the input names no held terminal follow"));
                    }
                    Some(None) => {
                        return Some(closed(id, "rejected", "input reaches only a terminal this host owns"));
                    }
                    Some(Some(_)) => {}
                }
                if self.open.values().any(|input| input.follow == follow) {
                    return Some(closed(id, "rejected", "the terminal follow already has an open input"));
                }
                self.open.insert(
                    id.to_owned(),
                    OpenInput {
                        follow: follow.to_owned(),
                        next_seq: 0,
                    },
                );
                tracing::info!(input = id, follow, actor = %session.actor, "terminal input opened");
                Some(json!({"kind":"input-opened", "id":id, "follow":follow, "next_seq":0}))
            }
            "input-close" => {
                self.open.remove(id);
                None
            }
            "input" => {
                // An input already closed told the client so; batches still on the way are dropped.
                let input = self.open.get(id)?;
                let Some(seq) = request.seq else {
                    self.open.remove(id);
                    return Some(closed(id, "rejected", "an input batch carries `seq`"));
                };
                if seq < input.next_seq {
                    return Some(json!({"kind":"input-ack", "id":id, "seq":seq}));
                }
                if seq > input.next_seq {
                    let expected = input.next_seq;
                    self.open.remove(id);
                    return Some(closed(id, "gap", format!("expected seq {expected}, got {seq}")));
                }
                let bytes = match request.data.as_ref().ok_or_else(|| "an input batch carries `data`".to_owned()).and_then(TerminalInputData::bytes) {
                    Ok(bytes) => bytes,
                    Err(message) => {
                        self.open.remove(id);
                        return Some(closed(id, "rejected", message));
                    }
                };
                let target = self.targets.get(&input.follow).cloned().flatten()?;
                let (state, session) = (state.clone(), session.clone());
                let written = tokio::task::spawn_blocking(move || write_batch(&state, &session, &target, &bytes))
                    .await
                    .unwrap_or_else(|error| Err(("rejected", error.to_string())));
                match written {
                    Ok(()) => {
                        self.open.get_mut(id)?.next_seq += 1;
                        Some(json!({"kind":"input-ack", "id":id, "seq":seq}))
                    }
                    Err((reason, message)) => {
                        self.open.remove(id);
                        tracing::info!(input = id, reason, %message, "terminal input closed");
                        Some(closed(id, reason, message))
                    }
                }
            }
            _ => Some(closed(id, "rejected", "unknown input command")),
        }
    }
}

/// Write one batch, after checking that the device is still paired, the viewer still holds its
/// record and the terminal still runs the incarnation the follow fenced. The error is the
/// reason the input closes.
fn write_batch(
    state: &AppState,
    session: &ClientSession,
    target: &InputTarget,
    bytes: &[u8],
) -> Result<(), (&'static str, String)> {
    let rejected = |error: ApiError| ("rejected", error.message);
    if let Some(pairing) = &session.pairing
        && pairing_withdrawn(state, pairing).map_err(rejected)?
    {
        return Err(("revoked", "the client credential was revoked or expired".into()));
    }
    let head = state
        .store
        .claims_for(&target.attachment, None)
        .map_err(|error| ("rejected", error.to_string()))?;
    if head.last().map(|claim| claim.id.as_str()) != Some(target.consumed.as_str()) {
        return Err(("detached", "the terminal viewer was detached".into()));
    }
    let live = terminal_live_session(state, &target.subject, Some(&target.incarnation)).map_err(
        |error| match error.code.as_str() {
            "stale-fence" | "not-found" => ("incarnation-changed", error.message),
            _ => ("rejected", error.message),
        },
    )?;
    daemon_pty(state)
        .and_then(|runtime| runtime.send_raw_if(&live.runtime_id, bytes, Some(&target.incarnation)))
        .map_err(|error| ("rejected", error.to_string()))
}
