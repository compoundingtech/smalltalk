//! st's Claude channel: daemon push input, native transcript receipts, and no file mailbox.
use crate::client::Client;
use crate::mailbox::{Fence, Frame, Receipt, Subscription};
use crate::model::{ClaimInput, ClaimRecord, MessageView};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt as _;

#[derive(Default, Deserialize, Serialize)]
struct State {
    initialized: bool,
    fence: Fence,
    attempted: BTreeSet<String>,
    confirmed: BTreeSet<String>,
    #[serde(default)]
    accepted: BTreeSet<String>,
    lines: st_drivers::reexec::LineBuffer,
}
#[derive(Default, Deserialize, Serialize)]
struct Handoffs {
    incarnation: String,
    attempted: BTreeSet<String>,
    confirmed: BTreeSet<String>,
    #[serde(default)]
    accepted: BTreeSet<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &Client,
    subject: &str,
    incarnation: &str,
    paths: &st_drivers::driver_paths::Paths,
    identity: &str,
    runtime_id: &str,
) -> Result<()> {
    let agent_dir = &paths.agent_dir;
    let ledger_path = agent_dir.join("native-channel-handoffs.json");
    let mut state = if let Some(path) =
        st_drivers::reexec::resume_path(st_drivers::reexec::CHANNEL_RESUME_ENV)
    {
        let state = st_drivers::reexec::read_state::<State>(&path);
        st_drivers::reexec::unblock_stop_signals();
        state.context("resuming st's Claude channel")?
    } else {
        let ledger = std::fs::read(&ledger_path)
            .ok()
            .map(|bytes| serde_json::from_slice::<Handoffs>(&bytes))
            .transpose()?;
        // Keep uncertain native handoffs across a seat restart. Proof is recovered
        // before any new offer; absence of proof gets a typed diagnostic, never an
        // unconditional replay of instructions whose consumption is uncertain.
        let ledger = ledger.unwrap_or_default();
        State {
            fence: Fence::new(subject, incarnation, "delivery"),
            attempted: ledger.attempted,
            confirmed: ledger.confirmed,
            accepted: ledger.accepted,
            ..State::default()
        }
    };
    state.fence.bind(client).await?;
    let report = || {
        json!({"transport":"claude-channel", "pid":std::process::id(),
        "image":st_drivers::reexec::running_identity().map(|i| i.token()),
        "follows":st_drivers::reexec::installed_binary().map(|path| path.display().to_string()),
        "channel":{"pid":std::process::id(),"image":st_drivers::reexec::running_identity().map(|i| i.token()),"age_ms":0},
        "ready":state.initialized})
    };
    let mut subscription = Subscription::start(client.clone(), state.fence.clone(), report());
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let spawn_reader =
        |sender: tokio::sync::mpsc::UnboundedSender<st_drivers::reexec::StdinChunk>| {
            st_drivers::reexec::StdinReader::spawn(move |chunk| sender.send(chunk).is_ok())
        };
    let mut reader = Some(spawn_reader(input_tx.clone()));
    let mut watch = st_drivers::reexec::ReplacementWatch::for_current_process();
    let mut stdout = tokio::io::stdout();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut messages = Vec::<MessageView>::new();
    let mut replayed = false;
    let mut content = BTreeMap::<String, String>::new();
    let mut transcript = Transcript::default();
    let wrapper = std::env::var(st_drivers::claude_session::SESSION_ENV).unwrap_or_default();
    let mut retries = BTreeMap::<String, Retry>::new();
    let mut unconfirmed_since = BTreeMap::<String, Instant>::new();
    let mut diagnostics = BTreeMap::<String, DiagnosticReport>::new();
    loop {
        tokio::select! {
            frame = subscription.receiver.recv() => match frame {
                Some(Frame::Mailbox { messages: next }) => { messages = next; replayed = true; },
                Some(Frame::Drain { operation }) => subscription.acknowledge_drain(operation),
                Some(Frame::Seat { .. }) => {}, // the outer driver owns the PTY title
                Some(Frame::Fenced { reason }) => anyhow::bail!("{reason}"),
                None => return Ok(()),
            },
            chunk = input_rx.recv() => match chunk {
                Some(st_drivers::reexec::StdinChunk::Bytes(bytes)) => {
                    state.lines.push(&bytes);
                    while let Some(line) = state.lines.next_line() {
                        if let Some(response) = request(&line, &mut state.initialized)? {
                            write(&mut stdout, &response).await?;
                        }
                    }
                    if state.initialized {
                        // MCP initialization is positive native readiness, independent of hooks.
                        let _: Result<ClaimRecord> = client.post("/v1/claims", &ClaimInput {
                            subject: subject.into(), kind:"harness.observed".into(), actor:Some(subject.into()),
                            fields:{let mut fields=BTreeMap::from([("state".into(),json!("ready")),("driver".into(),json!("claude")),
                                ("transport".into(),json!("claude-channel")),("incarnation_id".into(),json!(incarnation))]);
                                crate::suspension::annotate_quiescence(&mut fields); fields},
                            evidence:Vec::new(),expected_subject:None,idempotency_key:Some(format!("channel-ready:{subject}:{incarnation}:{}",state.fence.epoch)),
                        }).await;
                    }
                },
                Some(st_drivers::reexec::StdinChunk::Eof) | None => return Ok(()),
                Some(st_drivers::reexec::StdinChunk::Failed(error)) => return Err(error.into()),
            },
            _ = interval.tick() => {
                subscription.report(json!({"transport":"claude-channel", "pid":std::process::id(),
                    "image":st_drivers::reexec::running_identity().map(|i| i.token()),
                    "follows":st_drivers::reexec::installed_binary().map(|path| path.display().to_string()),
                    "channel":{"pid":std::process::id(),"image":st_drivers::reexec::running_identity().map(|i| i.token()),"age_ms":0},
                    "ready":state.initialized}));
                if state.initialized && replayed {
                    // The stream grants eligibility only to live mail. Retained handoffs can
                    // still acquire missing receipts from native proof, without another offer.
                    let live: BTreeSet<_> = messages.iter().map(|message| message.subject.clone()).collect();
                    let mut tracked = messages.clone();
                    let mut dirty = false;
                    let pending: BTreeSet<_> = state.attempted.iter().chain(&state.accepted).chain(&state.confirmed).cloned().collect();
                    for message in pending {
                        if live.contains(&message) { continue; }
                        let key = format!("recovery:{message}");
                        if !retry_ready(&retries, &key) { continue; }
                        match client.get::<MessageView>(&format!("/v1/messages/read/{message}")).await {
                            Ok(view) if view.to == subject && matches!(view.status.as_str(), "sent" | "staged" | "delivered") => {
                                tracked.push(view);
                                retries.remove(&key);
                            },
                            Ok(_) => {
                                dirty |= state.attempted.remove(&message);
                                dirty |= state.accepted.remove(&message);
                                dirty |= state.confirmed.remove(&message);
                                content.remove(&message);
                                unconfirmed_since.remove(&message);
                                retries.remove(&key);
                            },
                            Err(error) => {
                                retry_failed(&mut retries, &key);
                                diagnostic(client, &state.fence, &message, "claude-handoff-status-unavailable",
                                    &format!("{message}: retained handoff status could not be read; receipt recovery retries with backoff up to 30s: {error:#}"),
                                    &mut diagnostics).await;
                            },
                        }
                    }
                    for message in &tracked {
                        if !matches!(message.status.as_str(), "sent" | "staged" | "delivered") { continue; }
                        if content.contains_key(&message.subject) || !retry_ready(&retries, &message.subject) { continue; }
                        let prepared = async {
                            if !state.attempted.contains(&message.subject)
                                && !prepare_handoff(client, &state.fence, message).await? {
                                return Ok(None);
                            }
                            let body = body(client, message).await?;
                            let attachments = crate::blobs::materialize_for_seat(client, subject, &agent_dir.join("attachments"), message).await?;
                            Ok::<_, anyhow::Error>(Some(st_drivers::ding::with_dictation_notice(
                                st_drivers::ding::st3_notification_with_attachments(&message.subject, &message.from, &message.to,
                                    message.title.as_deref(), &body, &st_drivers::ding::st3_body_sha256(&body), &attachments), &message.tags)))
                        }.await;
                        match prepared {
                            Ok(Some(envelope)) => {
                                // Proof may predate this incarnation or arrive while a body is
                                // unavailable. Inspect it before authorizing another notification.
                                transcript.body_available(state.attempted.contains(&message.subject) || message.status != "sent");
                                content.insert(message.subject.clone(), envelope);
                                retries.remove(&message.subject);
                            },
                            Ok(None) => {},
                            Err(error) => {
                                retry_failed(&mut retries, &message.subject);
                                diagnostic(client, &state.fence, &message.subject, "claude-message-unforwarded",
                                    &format!("{message}: preparation failed; retrying with backoff up to 30s: {error:#}", message=message.subject),
                                    &mut diagnostics).await;
                            },
                        }
                    }
                    // User meta records and explicit mid-turn absorption are native
                    // consumption proof. An enqueue or writing stdout is not a receipt.
                    if !content.is_empty() && retry_ready(&retries, "transcript") {
                        let result = (|| {
                            let path = st_drivers::claude_session::channel_transcript_recovering(paths, identity, runtime_id, &wrapper)?
                                .context("Claude has not bound this wrapper to a native transcript")?;
                            transcript.appended(&path, |record| {
                                for (message, envelope) in &content {
                                    if native_receipt(record, envelope) {
                                        dirty |= state.confirmed.insert(message.clone());
                                        dirty |= state.accepted.insert(message.clone());
                                        dirty |= state.attempted.insert(message.clone());
                                    } else if native_acceptance(record, envelope) {
                                        dirty |= state.accepted.insert(message.clone());
                                        dirty |= state.attempted.insert(message.clone());
                                    }
                                }
                            })
                        })();
                        match result {
                            Ok(()) => {
                                retries.remove("transcript");
                            },
                            Err(error) => {
                                retry_failed(&mut retries, "transcript");
                                diagnostic(client, &state.fence, "transcript", "claude-receipt-unavailable",
                                    &format!("Native receipt lookup failed; retrying with backoff up to 30s: {error:#}"),
                                    &mut diagnostics).await;
                            },
                        }
                    }
                    for message in &tracked {
                        let Some(envelope) = content.get(&message.subject) else { continue; };
                        if state.attempted.contains(&message.subject) {
                            let since = unconfirmed_since.entry(message.subject.clone()).or_insert_with(Instant::now);
                            if !state.confirmed.contains(&message.subject) && message.status != "delivered" && since.elapsed() >= Duration::from_secs(30) {
                                diagnostic(client, &state.fence, &message.subject, "claude-handoff-unconfirmed",
                                    &format!("{} was offered to Claude but has no native consumption proof after 30s; retaining staged mail and checking receipts with bounded backoff. Repeating an uncertain notification is held to avoid duplicate instructions.", message.subject),
                                    &mut diagnostics).await;
                            }
                            continue;
                        }
                        if !live.contains(&message.subject) { continue; }
                        // Persist the handoff before stdout. A broken pipe cannot authorize
                        // repeating an uncertain notification. Restart first checks native proof.
                        state.attempted.insert(message.subject.clone());
                        save_handoffs(&ledger_path, &state)?;
                        unconfirmed_since.insert(message.subject.clone(), Instant::now());
                        write(&mut stdout, &json!({"jsonrpc":"2.0","method":"notifications/claude/channel",
                            "params":{"content":envelope,"meta":{"from":message.from,"messageId":message.subject,
                            "threadId":message.in_reply_to.as_ref().unwrap_or(&message.subject),"identity":identity}}})).await?;
                    }
                    // Persist native proof before trying receipts: a lost daemon acknowledgement
                    // must retry the receipt, never the notification.
                    if dirty { save_handoffs(&ledger_path, &state)?; }
                    dirty = false;
                    for message in state.accepted.union(&state.confirmed).cloned().collect::<Vec<_>>() {
                        let key = format!("receipt:{message}");
                        if !retry_ready(&retries, &key) { continue; }
                        let result = async {
                            receipt(client, &state.fence, &message, "delivered").await?;
                            if state.confirmed.contains(&message) {
                                receipt(client, &state.fence, &message, "read").await?;
                            }
                            Ok::<_, anyhow::Error>(())
                        }.await;
                        match result {
                            Ok(()) => {
                                state.accepted.remove(&message);
                                if state.confirmed.remove(&message) { content.remove(&message); }
                                retries.remove(&key);
                                unconfirmed_since.remove(&message);
                                dirty = true;
                            },
                            Err(error) => {
                                retry_failed(&mut retries, &key);
                                diagnostic(client, &state.fence, &message, "claude-receipt-unsettled",
                                    &format!("{message} has durable native proof but publishing its receipt failed; retrying with backoff up to 30s: {error:#}"),
                                    &mut diagnostics).await;
                            },
                        }
                    }
                    if dirty { save_handoffs(&ledger_path, &state)?; }
                }
                if let Some(binary) = watch.as_mut().and_then(|watch| tokio::task::block_in_place(|| watch.ready())) {
                    if let Some(reader) = reader.take() { tokio::task::block_in_place(|| reader.stop()); }
                    while let Ok(chunk) = input_rx.try_recv() {
                        match chunk {
                            st_drivers::reexec::StdinChunk::Bytes(bytes) => state.lines.push(&bytes),
                            st_drivers::reexec::StdinChunk::Eof => return Ok(()),
                            st_drivers::reexec::StdinChunk::Failed(error) => return Err(error.into()),
                        }
                    }
                    while let Some(line) = state.lines.next_line() {
                        if let Some(response) = request(&line, &mut state.initialized)? { write(&mut stdout, &response).await?; }
                    }
                    stdout.flush().await?;
                    let path = st_drivers::reexec::write_state(agent_dir, "st-channel-resume", &state)?;
                    let _ = st_drivers::reexec::exec(&binary, st_drivers::reexec::CHANNEL_RESUME_ENV, &path, &[]);
                    let _ = std::fs::remove_file(path);
                    if let Some(watch) = &mut watch { watch.refuse_current(); }
                    reader = Some(spawn_reader(input_tx.clone()));
                }
            },
        }
    }
}

#[derive(Default)]
struct Retry {
    failures: u32,
    due: Option<Instant>,
}
fn retry_ready(retries: &BTreeMap<String, Retry>, key: &str) -> bool {
    retries
        .get(key)
        .is_none_or(|retry| retry.due.is_none_or(|due| Instant::now() >= due))
}
fn retry_failed(retries: &mut BTreeMap<String, Retry>, key: &str) {
    let retry = retries.entry(key.into()).or_default();
    retry.failures = retry.failures.saturating_add(1);
    let seconds = (1_u64 << retry.failures.saturating_sub(1).min(5)).min(30);
    retry.due = Some(Instant::now() + Duration::from_secs(seconds));
}
struct DiagnosticReport {
    reason: String,
    recorded: bool,
}
async fn diagnostic(
    client: &Client,
    fence: &Fence,
    message: &str,
    code: &str,
    reason: &str,
    reports: &mut BTreeMap<String, DiagnosticReport>,
) {
    let key = format!("{code}:{}:{}:{message}", fence.subject, fence.incarnation);
    // A lost acknowledgement retries the same diagnostic payload, even when a
    // later attempt encounters a different transient failure.
    let report = reports
        .entry(key.clone())
        .or_insert_with(|| DiagnosticReport {
            reason: reason.into(),
            recorded: false,
        });
    if report.recorded {
        return;
    }
    let result: Result<ClaimRecord> = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: fence.subject.clone(),
                kind: "harness.diagnostic".into(),
                actor: Some(fence.subject.clone()),
                fields: BTreeMap::from([
                    ("severity".into(), json!("warning")),
                    ("status".into(), json!("waiting")),
                    ("code".into(), json!(code)),
                    ("reason".into(), json!(report.reason)),
                    ("incarnation_id".into(), json!(fence.incarnation)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.clone()),
            },
        )
        .await;
    if result.is_ok() {
        report.recorded = true;
    }
}

fn save_handoffs(path: &Path, state: &State) -> Result<()> {
    use std::io::Write as _;
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("handoff ledger parent")?)?;
    serde_json::to_writer(
        &mut file,
        &Handoffs {
            incarnation: state.fence.incarnation.clone(),
            attempted: state.attempted.clone(),
            confirmed: state.confirmed.clone(),
            accepted: state.accepted.clone(),
        },
    )?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
#[derive(Default)]
struct Transcript {
    path: std::path::PathBuf,
    offset: u64,
    lines: st_drivers::reexec::LineBuffer,
    #[cfg(test)]
    bytes_read: u64,
    #[cfg(test)]
    largest_read: usize,
}
impl Transcript {
    fn body_available(&mut self, uncertain: bool) {
        if uncertain {
            // This body may match proof before the current offset. Retain the
            // historical recovery scan; normal following never rewinds.
            self.offset = 0;
            self.lines = Default::default();
        }
    }
    fn appended(&mut self, path: &Path, mut record: impl FnMut(&Value)) -> Result<()> {
        use std::io::{Read as _, Seek as _};
        let mut file = std::fs::File::open(path)?;
        if self.path != path || file.metadata()?.len() < self.offset {
            self.path = path.to_owned();
            self.offset = 0;
            self.lines = Default::default();
        }
        file.seek(std::io::SeekFrom::Start(self.offset))?;
        // Drain complete records before reading another bounded chunk. Only a
        // partial trailing record carries over; records larger than a chunk still
        // grow normally, preserving the oversized-record support from #1347.
        let mut bytes = [0; 64 * 1024];
        loop {
            let read = match file.read(&mut bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if read == 0 {
                break;
            }
            self.offset += read as u64;
            #[cfg(test)]
            {
                self.bytes_read += read as u64;
                self.largest_read = self.largest_read.max(read);
            }
            self.lines.push(&bytes[..read]);
            while let Some(line) = self.lines.next_line() {
                if let Ok(value) = serde_json::from_str(&line) {
                    record(&value);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn records(&mut self, path: &Path) -> Result<Vec<Value>> {
        let mut records = Vec::new();
        self.appended(path, |record| records.push(record.clone()))?;
        Ok(records)
    }
}
fn native_acceptance(record: &Value, envelope: &str) -> bool {
    record["type"] == "queue-operation"
        && record["operation"] == "enqueue"
        && record["content"]
            .as_str()
            .is_some_and(|text| text.contains(envelope))
}

fn native_receipt(record: &Value, envelope: &str) -> bool {
    (record["type"] == "user"
        && record.pointer("/message/role").and_then(Value::as_str) == Some("user")
        && user_text(record).iter().any(|text| text.contains(envelope)))
        || (record["type"] == "queue-operation"
            && record["operation"] == "remove"
            && record["reason"] == "absorbed_mid_turn"
            && record["commandUuid"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && record["deliveryId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && record["content"]
                .as_str()
                .is_some_and(|text| text.contains(envelope)))
}

fn user_text(record: &Value) -> Vec<&str> {
    let content = &record["message"]["content"];
    match content {
        Value::String(text) => vec![text],
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => Vec::new(),
    }
}
async fn body(client: &Client, message: &MessageView) -> Result<String> {
    if message.content.starts_with("doc/") {
        let value: Value = client
            .get(&format!(
                "/v1/documents/content?reference={}",
                urlencoding::encode(&message.content)
            ))
            .await?;
        Ok(String::from_utf8(serde_json::from_value(
            value["bytes"].clone(),
        )?)?)
    } else {
        Ok(message.content.clone())
    }
}
// Only read/closed settles graph mail. Stream eligibility authorizes new offers;
// retained handoffs are prepared exclusively to recover their native receipts.
async fn prepare_handoff(client: &Client, fence: &Fence, message: &MessageView) -> Result<bool> {
    match message.status.as_str() {
        "sent" => Ok(receipt(client, fence, &message.subject, "staged")
            .await?
            .kind
            == "message.staged"),
        "staged" | "delivered" => Ok(true),
        _ => Ok(false),
    }
}

async fn receipt(
    client: &Client,
    fence: &Fence,
    message: &str,
    lifecycle: &str,
) -> Result<ClaimRecord> {
    client
        .post(
            "/v1/mailbox/receipts",
            &Receipt {
                fence: fence.clone(),
                message: message.into(),
                lifecycle: lifecycle.into(),
            },
        )
        .await
}
async fn write(stdout: &mut tokio::io::Stdout, frame: &Value) -> Result<()> {
    stdout
        .write_all(format!("{}\n", serde_json::to_string(frame)?).as_bytes())
        .await?;
    stdout.flush().await?;
    Ok(())
}
fn request(line: &str, initialized: &mut bool) -> Result<Option<Value>> {
    let request: Value = serde_json::from_str(line)?;
    let id = &request["id"];
    let result = match request["method"].as_str() {
        Some("initialize") => {
            json!({"protocolVersion":"2025-03-26","capabilities":{"experimental":{"claude/channel":{}}},
            "serverInfo":{"name":"st","version":env!("CARGO_PKG_VERSION")}})
        }
        Some("notifications/initialized") => {
            *initialized = true;
            return Ok(None);
        }
        Some("tools/list") => json!({"tools":[]}),
        Some("resources/list") => json!({"resources":[]}),
        Some("prompts/list") => json!({"prompts":[]}),
        Some("ping") => json!({}),
        _ => return Ok(None),
    };
    Ok((!id.is_null()).then(|| json!({"jsonrpc":"2.0","id":id,"result":result})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn claude_receipt_state_accepts_ledgers_written_before_native_acceptance_tracking() {
        let mut state = serde_json::to_value(State::default()).unwrap();
        state.as_object_mut().unwrap().remove("accepted");
        assert!(
            serde_json::from_value::<State>(state)
                .unwrap()
                .accepted
                .is_empty()
        );
        let handoffs: Handoffs = serde_json::from_value(json!({
            "incarnation":"previous", "attempted":["message/quartz"], "confirmed":[],
        }))
        .unwrap();
        assert!(handoffs.accepted.is_empty());
        assert!(handoffs.attempted.contains("message/quartz"));
    }

    #[tokio::test]
    async fn claude_receipt_preparation_needs_no_backward_transition_and_read_or_closed_mail_is_final()
     {
        let client = Client::new(crate::client::Endpoint::Unix(std::path::PathBuf::from(
            "/absent-st889-daemon.sock",
        )));
        let fence = Fence::new("agent/eval.worker", "new-incarnation", "delivery");
        for (status, expected) in [
            ("staged", true),
            ("delivered", true),
            ("read", false),
            ("closed", false),
        ] {
            let message = MessageView {
                subject: "message/replay".into(),
                from: "person/eval".into(),
                to: fence.subject.clone(),
                content: "Signal".into(),
                status: status.into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                created_index: 1,
                attachments: Vec::new(),
            };
            assert_eq!(
                prepare_handoff(&client, &fence, &message).await.unwrap(),
                expected,
                "{status}"
            );
        }
    }

    #[test]
    fn claude_native_proof_requires_the_exact_envelope_in_a_user_record() {
        let envelope = st_drivers::ding::st3_notification_text(
            "message/quartz",
            "person/eval",
            "agent/eval.worker",
            Some("Signal"),
            "QUARTZ SIGNAL",
            &st_drivers::ding::st3_body_sha256("QUARTZ SIGNAL"),
        );
        assert!(!native_receipt(
            &json!({"type":"assistant","message":{"role":"assistant","content":envelope}}),
            &envelope
        ));
        assert!(!native_receipt(
            &json!({"type":"user","message":{"role":"user","content":"QUARTZ SIGNAL"}}),
            &envelope
        ));
        assert!(native_receipt(
            &json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":envelope}]}}),
            &envelope
        ));
        let enqueue = json!({"type":"queue-operation","operation":"enqueue","content":envelope});
        assert!(native_acceptance(&enqueue, &envelope));
        assert!(!native_receipt(&enqueue, &envelope));
        let mut remove = json!({"type":"queue-operation","operation":"remove", "reason":"absorbed_mid_turn",
            "content":envelope,"commandUuid":"native-command","deliveryId":"native-delivery"});
        assert!(native_receipt(&remove, &envelope));
        remove["reason"] = json!("cancelled");
        assert!(!native_receipt(&remove, &envelope));
        remove["reason"] = json!("absorbed_mid_turn");
        remove["content"] = json!("QUARTZ SIGNAL");
        assert!(!native_receipt(&remove, &envelope));
    }

    #[test]
    fn claude_transcript_tails_partial_records_and_rebinds_without_receipt_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"{\"type\":\"user\",\"message\":").unwrap();
        let mut transcript = Transcript::default();
        assert!(transcript.records(&path).unwrap().is_empty());
        file.write_all(b"{\"role\":\"user\",\"content\":\"QUARTZ\"}}\n")
            .unwrap();
        let records = transcript.records(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert!(native_receipt(&records[0], "QUARTZ"));
        assert!(transcript.records(&path).unwrap().is_empty());
        std::fs::write(&path, b"{\"type\":\"user\"}\n").unwrap();
        assert_eq!(transcript.records(&path).unwrap().len(), 1);
        let rebound = root.path().join("next-native.jsonl");
        std::fs::write(&rebound, b"{\"type\":\"user\"}\n").unwrap();
        assert_eq!(transcript.records(&rebound).unwrap().len(), 1);
        assert!(!root.path().join("resources").exists());
    }

    #[test]
    fn claude_revisits_native_proof_when_an_uncertain_body_becomes_available() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        let envelope = st_drivers::ding::st3_notification_text(
            "message/quartz",
            "person/eval",
            "agent/eval.worker",
            None,
            "QUARTZ SIGNAL",
            &st_drivers::ding::st3_body_sha256("QUARTZ SIGNAL"),
        );
        let record = json!({"type":"user","message":{"role":"user","content":envelope}});
        std::fs::write(&path, format!("{record}\n")).unwrap();
        let mut transcript = Transcript::default();
        // A different available message caused the initial scan after reexec.
        transcript.records(&path).unwrap();
        assert!(transcript.records(&path).unwrap().is_empty());
        transcript.body_available(true);
        assert!(native_receipt(
            &transcript.records(&path).unwrap()[0],
            &envelope
        ));
        transcript.body_available(false);
        assert!(transcript.records(&path).unwrap().is_empty());
        assert!(!root.path().join("resources").exists());
    }

    #[test]
    fn claude_large_transcript_reads_only_new_bytes_per_append() {
        use std::io::BufWriter;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large-native.jsonl");
        let mut file = BufWriter::new(std::fs::File::create(&path).unwrap());
        file.write_all(b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"QUARTZ HISTORICAL\"}}\n")
            .unwrap();
        let mut line =
            serde_json::to_vec(&json!({"type":"assistant","padding":"x".repeat(4000)})).unwrap();
        line.push(b'\n');
        let count = (100_usize * 1024 * 1024).div_ceil(line.len());
        for _ in 0..count {
            file.write_all(&line).unwrap();
        }
        file.flush().unwrap();
        let initial_size = std::fs::metadata(&path).unwrap().len();
        let mut transcript = Transcript::default();
        let started = Instant::now();
        let mut seen = 0;
        transcript.appended(&path, |_| seen += 1).unwrap();
        let scan_time = started.elapsed();
        assert_eq!(seen, count + 1);
        assert_eq!(transcript.bytes_read, initial_size);
        assert!(transcript.largest_read <= 64 * 1024);
        // The old splitter shifts over a terabyte for this input. Keep a wide
        // margin over a linear scan, alongside the deterministic work counters
        // in LineBuffer's tests and the exact per-append read counts below.
        assert!(
            scan_time < Duration::from_secs(30),
            "initial scan took {scan_time:?}"
        );

        let receipt =
            b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"QUARTZ\"}}\n";
        let started = Instant::now();
        for _ in 0..1000 {
            let before = transcript.bytes_read;
            file.write_all(receipt).unwrap();
            file.flush().unwrap();
            let mut seen = 0;
            transcript
                .appended(&path, |record| {
                    assert!(native_receipt(record, "QUARTZ"));
                    seen += 1;
                })
                .unwrap();
            assert_eq!(seen, 1);
            assert_eq!(transcript.bytes_read - before, receipt.len() as u64);
            transcript
                .appended(&path, |_| panic!("idle poll replayed a record"))
                .unwrap();
            assert_eq!(transcript.bytes_read - before, receipt.len() as u64);
        }
        eprintln!(
            "transcript: {initial_size} initial bytes in {scan_time:?}; 1000 appends + idle polls, {} new bytes in {:?}",
            1000 * receipt.len(),
            started.elapsed()
        );
        // A bounded look-back window would miss this proof over 100 MiB behind
        // EOF. Late uncertain bodies must still recover it without another offer.
        transcript.body_available(true);
        let mut historical = 0;
        transcript
            .appended(&path, |record| {
                if native_receipt(record, "QUARTZ HISTORICAL") {
                    historical += 1;
                }
            })
            .unwrap();
        assert_eq!(historical, 1);
    }

    #[test]
    fn claude_transcript_preserves_large_partial_receipts_across_chunks_and_recovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("oversized-native.jsonl");
        let text = format!("{}QUARTZ", "x".repeat(2 * 1024 * 1024));
        let record = json!({"type":"user","message":{"role":"user","content":text}});
        let bytes = serde_json::to_vec(&record).unwrap();
        let split = bytes.len() / 2;
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&bytes[..split]).unwrap();
        let mut transcript = Transcript::default();
        assert!(transcript.records(&path).unwrap().is_empty());
        // Resetting for an uncertain handoff must discard the partial prefix and
        // recover all historical proof, including records larger than 1 MiB.
        transcript.body_available(true);
        assert!(transcript.records(&path).unwrap().is_empty());
        file.write_all(&bytes[split..]).unwrap();
        assert!(transcript.records(&path).unwrap().is_empty());
        file.write_all(b"\n").unwrap();
        let records = transcript.records(&path).unwrap();
        assert_eq!(records, [record]);
        assert!(native_receipt(&records[0], "QUARTZ"));
        assert!(transcript.records(&path).unwrap().is_empty());
    }

    #[test]
    fn claude_uncertain_and_confirmed_handoffs_survive_a_lost_receipt_ack_without_message_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native-channel-handoffs.json");
        let mut state = State {
            fence: Fence::new("agent/eval.worker", "session-1", "delivery"),
            ..State::default()
        };
        state.attempted.insert("message/quartz".into());
        save_handoffs(&path, &state).unwrap();
        let ledger: Handoffs = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(ledger.attempted.contains("message/quartz"));
        assert!(ledger.confirmed.is_empty());
        state.confirmed.insert("message/quartz".into());
        save_handoffs(&path, &state).unwrap();
        let ledger: Handoffs = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(ledger.confirmed.contains("message/quartz"));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert!(
            !std::fs::read_to_string(path)
                .unwrap()
                .contains("QUARTZ SIGNAL")
        );
    }
}
