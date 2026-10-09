//! Durable, harness-neutral conversation events produced by native drivers.
//!
//! The harness adapters see richer protocol events than st3's supervisor. They normalize those
//! events here into append/replace/finalize operations. Fresh st seats commit each operation to
//! the st-owned event outbox; legacy adopted providers and st2 retain a bounded record log. Both
//! transports preserve reported events across daemon restarts and keep provider transcript
//! formats out of the client API.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::flock::{FileLock, Mode, Open, open};
use crate::fsatomic::{self, Durability, Staging};

const SCHEMA: &str = "st.harness-timeline.v1";
const RECORD_NAME: &str = "harness-timeline";
const LOCK_NAME: &str = ".harness-timeline.lock";
const MAX_OPERATIONS: usize = 4_096;
const MAX_RECORD_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_STRING_CHARS: usize = 8_192;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub schema: String,
    pub driver: String,
    pub incarnation_id: String,
    pub next_sequence: u64,
    pub operations: Vec<Operation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub operation: String,
    pub entry_id: String,
    pub sequence: u64,
    pub revision: u64,
    pub role: String,
    pub entry_type: String,
    pub final_entry: bool,
    pub body: Value,
    pub driver: String,
    pub incarnation_id: String,
    pub observed_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
    Message,
    Content,
    ToolCall,
    ToolResult,
    Status,
    Error,
    Usage,
    Redaction,
    Truncation,
}

impl EntryType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Content => "content",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Status => "status",
            Self::Error => "error",
            Self::Usage => "usage",
            Self::Redaction => "redaction",
            Self::Truncation => "truncation",
        }
    }
}

pub struct Writer {
    path: PathBuf,
    lock_path: PathBuf,
    driver: String,
    incarnation_id: String,
    model: Option<String>,
    account: Option<String>,
    turn_models: BTreeMap<String, String>,
}

impl Writer {
    pub fn new(
        agent_dir: &Path,
        driver: impl Into<String>,
        incarnation_id: impl Into<String>,
    ) -> Self {
        Self {
            path: timeline_path(agent_dir),
            lock_path: agent_dir.join(LOCK_NAME),
            driver: driver.into(),
            incarnation_id: incarnation_id.into(),
            model: None,
            account: None,
            turn_models: BTreeMap::new(),
        }
    }

    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// The opaque paying account (see [`account_label`]) stamped on each response usage entry
    /// this writer appends from now on. A response records the account at the time it is
    /// written, so an account change never relabels earlier spend.
    pub fn set_account(&mut self, account: Option<String>) {
        self.account = account;
    }

    pub fn with_account(mut self, account: Option<String>) -> Self {
        self.account = account;
        self
    }

    pub fn remember_turn_model(&mut self, turn_id: &str, model: &str) {
        if self.turn_models.len() >= 128 {
            self.turn_models.pop_first();
        }
        self.turn_models.insert(turn_id.into(), model.into());
    }

    pub fn append(
        &mut self,
        source_id: impl Into<String>,
        role: Role,
        entry_type: EntryType,
        body: Value,
        final_entry: bool,
    ) -> Result<()> {
        self.write(source_id.into(), role, entry_type, body, final_entry)
    }

    fn write(
        &mut self,
        source_id: String,
        role: Role,
        entry_type: EntryType,
        body: Value,
        final_entry: bool,
    ) -> Result<()> {
        fs::create_dir_all(
            self.path
                .parent()
                .context("the harness timeline path has no parent")?,
        )?;
        let lock = open(&self.lock_path, Open::Create)?;
        let _held = FileLock::hold_blocking(lock, Mode::Exclusive)?;
        let event_dir = self.path.parent().unwrap();
        let event_transport = crate::harness_events::enabled(event_dir);
        let source_key = pseudonym("source", &source_id);
        let mut record = if event_transport {
            Some(crate::harness_events::timeline_for_write(
                event_dir,
                &self.driver,
                &self.incarnation_id,
                &source_key,
            )?)
        } else {
            read(&self.path)
        }
        .filter(|record| {
            record.driver == self.driver && record.incarnation_id == self.incarnation_id
        })
        .unwrap_or_else(|| Record {
            schema: crate::contracts::schema_for_session(
                self.path.parent().unwrap(),
                &self.incarnation_id,
                SCHEMA,
            ),
            driver: self.driver.clone(),
            incarnation_id: self.incarnation_id.clone(),
            next_sequence: 1,
            operations: Vec::new(),
        });

        let prior_operations = record.operations.len();
        anyhow::ensure!(
            matches!(
                self.driver.as_str(),
                "codex" | "claude" | "pi" | "omp" | "opencode"
            ),
            "unsupported harness timeline driver `{}`",
            self.driver
        );
        let source_id = pseudonym("source", &source_id);
        let (mut body, redacted_bytes, redacted_items, truncated_items) =
            normalize_body(entry_type, &source_id, body);
        if entry_type == EntryType::Usage {
            body["driver"] = Value::String(self.driver.clone());
            if body["semantics"] == "response"
                && body.get("account").is_none()
                && let Some(account) = &self.account
            {
                body["account"] = Value::String(account.clone());
            }
        }
        anyhow::ensure!(
            serde_json::to_vec(&body)?.len() <= MAX_BODY_BYTES,
            "normalized harness timeline body exceeds {MAX_BODY_BYTES} bytes"
        );
        // Channel state can be restated at every wake. Repeating an unchanged
        // status is not a conversation event, and otherwise crowds actual chat
        // out of bounded timeline pages while needlessly replicating claims.
        if entry_type == EntryType::Status
            && record.operations.last().is_some_and(|entry| {
                entry.entry_type == EntryType::Status.as_str() && entry.body == body
            })
        {
            return Ok(());
        }
        let prior = record
            .operations
            .iter()
            .rev()
            .find(|entry| entry.source_id.as_deref() == Some(source_id.as_str()));
        if prior.is_some_and(|entry| {
            entry.body == body
                && entry.final_entry == final_entry
                && entry.role == role.as_str()
                && entry.entry_type == entry_type.as_str()
        }) {
            return Ok(());
        }
        if prior.is_some_and(|entry| entry.final_entry) {
            return Ok(());
        }
        let sequence = prior.map_or_else(
            || {
                let sequence = record.next_sequence;
                record.next_sequence = record.next_sequence.saturating_add(1);
                sequence
            },
            |entry| entry.sequence,
        );
        let revision = prior.map_or(1, |entry| entry.revision.saturating_add(1));
        let operation = if prior.is_none() {
            "append"
        } else if final_entry {
            "finalize"
        } else {
            "replace"
        };
        let observed_at_unix_ms = now_ms();
        record.operations.push(Operation {
            operation: operation.into(),
            entry_id: stable_entry_id(&self.driver, &self.incarnation_id, &source_id),
            sequence,
            revision,
            role: role.as_str().into(),
            entry_type: entry_type.as_str().into(),
            final_entry,
            body,
            driver: self.driver.clone(),
            incarnation_id: self.incarnation_id.clone(),
            observed_at_unix_ms,
            source_id: Some(source_id.clone()),
        });
        if redacted_bytes > 0 || redacted_items > 0 {
            push_notice(
                &mut record,
                &self.driver,
                &self.incarnation_id,
                &format!("{source_id}:redaction"),
                EntryType::Redaction,
                json!({"reason": "sensitive-content", "withheld_bytes": redacted_bytes, "withheld_items": redacted_items}),
                observed_at_unix_ms,
            );
        }
        if truncated_items > 0 {
            let omitted = record.next_sequence;
            push_notice(
                &mut record,
                &self.driver,
                &self.incarnation_id,
                &format!("{source_id}:truncation"),
                EntryType::Truncation,
                json!({
                    "reason": "producer-bound",
                    "omitted_from_sequence": omitted,
                    "omitted_to_sequence": omitted,
                }),
                observed_at_unix_ms,
            );
        }
        if event_transport {
            return crate::harness_events::write_timeline(
                event_dir,
                &record,
                &record.operations[prior_operations..],
            );
        }
        let mut bytes = compact_to_bounds(&mut record)?;
        bytes.push(b'\n');
        fsatomic::replace(
            &self.path,
            &bytes,
            Staging::new(".harness-timeline"),
            Durability::FsyncFileAndDir,
        )
        .with_context(|| format!("publishing harness timeline {}", self.path.display()))
    }
}

fn push_notice(
    record: &mut Record,
    driver: &str,
    incarnation_id: &str,
    source_id: &str,
    entry_type: EntryType,
    body: Value,
    observed_at_unix_ms: u64,
) {
    if record
        .operations
        .iter()
        .any(|entry| entry.source_id.as_deref() == Some(source_id))
    {
        return;
    }
    let sequence = record.next_sequence;
    record.next_sequence = record.next_sequence.saturating_add(1);
    record.operations.push(Operation {
        operation: "append".into(),
        entry_id: stable_entry_id(driver, incarnation_id, source_id),
        sequence,
        revision: 1,
        role: Role::System.as_str().into(),
        entry_type: entry_type.as_str().into(),
        final_entry: true,
        body,
        driver: driver.into(),
        incarnation_id: incarnation_id.into(),
        observed_at_unix_ms,
        source_id: Some(source_id.into()),
    });
}

pub fn timeline_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(RECORD_NAME)
}

pub fn read(path: &Path) -> Option<Record> {
    if let Some(dir) = path.parent()
        && crate::harness_events::enabled(dir)
    {
        return crate::harness_events::read_timeline(dir).ok().flatten();
    }
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_RECORD_BYTES {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    let record = serde_json::from_slice::<Record>(&bytes).ok()?;
    validate_record(&record).then_some(record)
}

pub fn observe_codex(writer: &mut Writer, message: &Value, thread_id: &str) -> Result<()> {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    if message
        .pointer("/params/threadId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != thread_id)
    {
        return Ok(());
    }
    if method == "thread/tokenUsage/updated" {
        let Some(usage) = message.pointer("/params/tokenUsage/last") else {
            return Ok(());
        };
        let total = usage
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // A turn has several model responses. The session total advances for each
        // response, so it distinguishes updates even when their last buckets match.
        let session_total = message
            .pointer("/params/tokenUsage/total/totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(total);
        let turn_id = message
            .pointer("/params/turnId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let model = message
            .pointer("/params/model")
            .and_then(Value::as_str)
            .or_else(|| writer.turn_models.get(turn_id).map(String::as_str))
            .or(writer.model.as_deref())
            .map(str::to_owned);
        return writer.append(
            format!("codex:usage:{}:{session_total}", message.pointer("/params/turnId").and_then(Value::as_str).unwrap_or("unknown")),
            Role::System,
            EntryType::Usage,
            {
                // Codex counts cached and cache-written input inside `inputTokens` and reasoning
                // inside `outputTokens`. st's buckets are disjoint, as Claude reports them:
                // `input_tokens` is the uncached remainder, so the buckets sum to the total and
                // each one is priced once.
                let field = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                let cached = field("cachedInputTokens");
                let cache_writes = field("cacheWriteInputTokens");
                json!({
                    "semantics": "response", "driver": "codex",
                    "model": model,
                    "input_tokens": field("inputTokens").saturating_sub(cached).saturating_sub(cache_writes),
                    "output_tokens": field("outputTokens"),
                    "reasoning_tokens": field("reasoningOutputTokens"),
                    "cached_tokens": cached,
                    "cache_write_tokens": cache_writes,
                    "turn_id": message.pointer("/params/turnId"),
                    "total_tokens": total,
                })
            },
            true,
        );
    }
    if !matches!(method, "item/started" | "item/completed") {
        if method.contains("error") {
            return writer.append(
                source_id(message, "codex:error"),
                Role::System,
                EntryType::Error,
                json!({"code": "harness-error", "message": safe_summary(message), "retryable": false, "details": {}}),
                true,
            );
        }
        return Ok(());
    }
    let item = message.pointer("/params/item").unwrap_or(&Value::Null);
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| source_id(message, "codex:item"));
    let final_entry = method == "item/completed";
    match item_type {
        "userMessage" | "agentMessage" => {
            let role = if item_type == "userMessage" {
                Role::User
            } else {
                Role::Assistant
            };
            writer.append(
                format!("{item_id}:message"),
                role,
                EntryType::Message,
                json!({"message_id": item_id}),
                true,
            )?;
            if let Some(text) = item_text(item) {
                writer.append(
                    format!("{item_id}:content"),
                    role,
                    EntryType::Content,
                    json!({"media_type": "text/plain", "text": text}),
                    final_entry,
                )?;
            }
        }
        "commandExecution" | "mcpToolCall" => {
            let name = item
                .get("name")
                .or_else(|| item.get("command"))
                .and_then(Value::as_str)
                .unwrap_or(item_type);
            writer.append(format!("{item_id}:call"), Role::Assistant, EntryType::ToolCall, json!({"call_id": item_id, "name": name, "arguments": item.get("arguments").or_else(|| item.get("input")).cloned().unwrap_or(Value::Null)}), true)?;
            if final_entry {
                writer.append(format!("{item_id}:result"), Role::Tool, EntryType::ToolResult, json!({"call_id": item_id, "status": if item.get("error").is_some_and(|value| !value.is_null()) {"error"} else {"success"}, "media_type": "application/json", "content": item.get("output").or_else(|| item.get("result")).cloned().unwrap_or(Value::Null)}), true)?;
            }
        }
        // Provider reasoning is deliberately never projected as conversation content. Its
        // existence is useful, its hidden chain-of-thought is not client data.
        "reasoning" if final_entry => writer.append(
            format!("{item_id}:reasoning"),
            Role::System,
            EntryType::Redaction,
            json!({
                "reason": "hidden-provider-reasoning",
                "withheld_bytes": serde_json::to_vec(item).map_or(0, |bytes| bytes.len()),
            }),
            true,
        )?,
        _ => {}
    }
    Ok(())
}

pub fn observe_claude(writer: &mut Writer, event: &str, payload: &Value) -> Result<()> {
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match event {
        "UserPromptSubmit" => {
            let id = source_id(payload, &format!("claude:{session}:user"));
            writer.append(format!("{id}:message"), Role::User, EntryType::Message, json!({"message_id": id}), true)?;
            if let Some(text) = payload.get("prompt").and_then(Value::as_str) {
                writer.append(format!("{id}:content"), Role::User, EntryType::Content, json!({"media_type": "text/plain", "text": text}), true)?;
            }
        }
        "PreToolUse" => {
            let call = payload.get("tool_use_id").and_then(Value::as_str).unwrap_or("unknown");
            writer.append(format!("claude:{session}:{call}:call"), Role::Assistant, EntryType::ToolCall, json!({"call_id": call, "name": payload.get("tool_name").and_then(Value::as_str).unwrap_or("unknown"), "arguments": payload.get("tool_input").cloned().unwrap_or(Value::Null)}), true)?;
        }
        "PostToolUse" => {
            let call = payload.get("tool_use_id").and_then(Value::as_str).unwrap_or("unknown");
            writer.append(format!("claude:{session}:{call}:result"), Role::Tool, EntryType::ToolResult, json!({"call_id": call, "status": if payload.get("error").is_some_and(|value| !value.is_null()) {"error"} else {"success"}, "media_type": "application/json", "content": payload.get("tool_response").cloned().unwrap_or(Value::Null)}), true)?;
        }
        "Stop" => writer.append(format!("claude:{session}:stop:{}", source_id(payload, "stop")), Role::System, EntryType::Status, json!({"status": "completed"}), true)?,
        "StopFailure" => writer.append(format!("claude:{session}:failure:{}", source_id(payload, "failure")), Role::System, EntryType::Error, json!({"code": "harness-error", "message": safe_summary(payload), "retryable": false, "details": {}}), true)?,
        _ => {}
    }
    Ok(())
}

/// Claude's Stop hook has the exact native transcript path, while its event
/// payload does not contain the assistant's answer. Publish a small recent
/// answer window only at turn boundaries; never poll the growing transcript.
#[derive(Deserialize, Serialize)]
struct ClaudeTranscriptCursor {
    session_id: String,
    offset: u64,
}

pub fn observe_claude_stop_transcript(
    writer: &mut Writer,
    payload: &Value,
    home: &Path,
) -> Result<()> {
    let (Some(session_id), Some(transcript)) = (
        payload.get("session_id").and_then(Value::as_str),
        payload.get("transcript_path").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    let allowed = home.join(".claude/projects").canonicalize()?;
    let transcript = Path::new(transcript).canonicalize()?;
    anyhow::ensure!(
        transcript.starts_with(&allowed)
            && transcript
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            && transcript
                .file_stem()
                .is_some_and(|stem| stem == session_id),
        "Claude Stop transcript is not the exact session beneath the native projects directory"
    );
    let mut file = fs::File::open(&transcript)?;
    let length = file.metadata()?.len();
    let cursor_path = writer
        .path
        .with_file_name(".harness-timeline-claude-cursor");
    let start = fs::read(&cursor_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeTranscriptCursor>(&bytes).ok())
        .filter(|cursor| cursor.session_id == session_id && cursor.offset <= length)
        .map(|cursor| cursor.offset)
        .unwrap_or(0);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut offset = start;
    let mut index = 0_usize;
    let mut answers = BTreeMap::<String, (usize, String)>::new();
    let mut usage = BTreeMap::<String, (usize, Value)>::new();
    let mut turn_id = String::new();
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // A growing transcript may end in a partial JSON line. Read it on the next Stop.
            if !line.ends_with('\n') {
                break;
            }
            offset = offset.saturating_add(bytes as u64);
            continue;
        };
        offset = offset.saturating_add(bytes as u64);
        index = index.saturating_add(1);
        if value["type"] == "user"
            && value
                .get("sessionId")
                .and_then(Value::as_str)
                .is_none_or(|id| id == session_id)
            && !value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|parts| {
                    !parts.is_empty() && parts.iter().all(|part| part["type"] == "tool_result")
                })
        {
            if let Some(id) = value.get("uuid").and_then(Value::as_str) {
                turn_id = id.to_owned();
                usage.clear();
            }
        }
        if value["type"] != "assistant"
            || value
                .get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != session_id)
        {
            continue;
        }
        let Some(message_id) = value
            .pointer("/message/id")
            .and_then(Value::as_str)
            .or_else(|| value.get("uuid").and_then(Value::as_str))
        else {
            continue;
        };
        let turn = if turn_id.is_empty() {
            value
                .get("parentUuid")
                .and_then(Value::as_str)
                .unwrap_or("")
        } else {
            &turn_id
        };
        if let Some(body) = claude_usage_body(&value, turn) {
            usage.insert(message_id.to_owned(), (index, body));
        }
        let text = value
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            answers.insert(message_id.to_owned(), (index, text));
            if answers.len() > 8 {
                let oldest = answers
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(id, _)| id.clone());
                if let Some(oldest) = oldest {
                    answers.remove(&oldest);
                }
            }
        }
    }
    let mut answers = answers.into_iter().collect::<Vec<_>>();
    answers.sort_by_key(|(_, (index, _))| *index);
    for (message_id, (_, text)) in answers.into_iter().rev().take(8).rev() {
        writer.append(
            format!("claude:{session_id}:{message_id}:answer"),
            Role::Assistant,
            EntryType::Content,
            json!({"media_type":"text/plain","text":text}),
            true,
        )?;
    }
    for (message_id, (_, body)) in usage {
        writer.append(
            format!("claude:{session_id}:{message_id}:usage"),
            Role::System,
            EntryType::Usage,
            body,
            true,
        )?;
    }
    fs::write(
        &cursor_path,
        serde_json::to_vec(&ClaudeTranscriptCursor {
            session_id: session_id.into(),
            offset,
        })?,
    )?;
    observe_claude_subagent_transcripts(writer, &transcript, session_id)
}

/// One Claude response's token buckets, as Claude reports them: already disjoint.
fn claude_usage_body(value: &Value, turn_id: &str) -> Option<Value> {
    let tokens = value.pointer("/message/usage")?;
    let bucket = |key| tokens.get(key).and_then(Value::as_u64).unwrap_or(0);
    let input = bucket("input_tokens");
    let output = bucket("output_tokens");
    let cache_writes = bucket("cache_creation_input_tokens");
    let cache_reads = bucket("cache_read_input_tokens");
    // Claude splits cache writes by TTL; a one-hour write costs more than a five-minute one.
    let cache_writes_1h = tokens
        .pointer("/cache_creation/ephemeral_1h_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(cache_writes);
    Some(json!({
        "semantics": "response", "driver": "claude",
        "model": value.pointer("/message/model"),
        "turn_id": turn_id,
        "input_tokens": input, "output_tokens": output,
        "cache_write_tokens": cache_writes, "cache_write_1h_tokens": cache_writes_1h,
        "cached_tokens": cache_reads,
        "total_tokens": input.saturating_add(output).saturating_add(cache_writes).saturating_add(cache_reads),
    }))
}

/// Byte offsets already read in each of a session's subagent transcripts.
#[derive(Default, Deserialize, Serialize)]
struct ClaudeSubagentCursors {
    session_id: String,
    offsets: BTreeMap<String, u64>,
}

const MAX_SUBAGENT_TRANSCRIPTS: usize = 1_024;

/// Claude writes each subagent's requests to `SESSION/subagents/agent-*.jsonl` beside the main
/// transcript, so the main transcript alone misses that spend. Read each file's new complete
/// lines at the parent's Stop and record every response's usage once, with the subagent as its
/// turn.
fn observe_claude_subagent_transcripts(
    writer: &mut Writer,
    transcript: &Path,
    session_id: &str,
) -> Result<()> {
    let Some(directory) = transcript
        .parent()
        .map(|parent| parent.join(session_id).join("subagents"))
    else {
        return Ok(());
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return Ok(());
    };
    let cursor_path = writer
        .path
        .with_file_name(".harness-timeline-claude-subagent-cursors");
    let mut cursors = fs::read(&cursor_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ClaudeSubagentCursors>(&bytes).ok())
        .filter(|cursors| cursors.session_id == session_id)
        .unwrap_or_else(|| ClaudeSubagentCursors {
            session_id: session_id.into(),
            offsets: BTreeMap::new(),
        });
    let mut files = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (name.starts_with("agent-") && name.ends_with(".jsonl"))
                .then(|| Some((name, entry.metadata().ok()?.len())))?
        })
        .collect::<Vec<_>>();
    files.sort();
    files.truncate(MAX_SUBAGENT_TRANSCRIPTS);
    cursors
        .offsets
        .retain(|name, _| files.iter().any(|(file, _)| file == name));
    for (name, length) in files {
        let start = cursors
            .offsets
            .get(&name)
            .copied()
            .filter(|offset| *offset <= length)
            .unwrap_or(0);
        if start == length {
            continue;
        }
        let agent = name.trim_start_matches("agent-").trim_end_matches(".jsonl");
        let mut file = fs::File::open(directory.join(&name))?;
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        let mut offset = start;
        let mut usage = BTreeMap::<String, Value>::new();
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 || !line.ends_with('\n') {
                // A partial last line is read again at the next Stop.
                break;
            }
            offset = offset.saturating_add(bytes as u64);
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] != "assistant" {
                continue;
            }
            let Some(message_id) = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .or_else(|| value.get("uuid").and_then(Value::as_str))
            else {
                continue;
            };
            if let Some(body) = claude_usage_body(&value, &format!("subagent:{agent}")) {
                usage.insert(message_id.to_owned(), body);
            }
        }
        for (message_id, body) in usage {
            writer.append(
                format!("claude:{session_id}:{message_id}:usage"),
                Role::System,
                EntryType::Usage,
                body,
                true,
            )?;
        }
        cursors.offsets.insert(name, offset);
    }
    fs::write(&cursor_path, serde_json::to_vec(&cursors)?)?;
    Ok(())
}

pub fn observe_channel_frame(writer: &mut Writer, frame: &Value) -> Result<()> {
    match frame.get("type").and_then(Value::as_str) {
        Some("state") => {
            let status = if frame.get("state").and_then(Value::as_str) == Some("active") {
                "running"
            } else {
                "waiting"
            };
            return writer.append(
                source_id(frame, "channel:status"),
                Role::System,
                EntryType::Status,
                json!({"status": status}),
                true,
            );
        }
        Some("context") => {
            let Some(reading) = frame.get("reading") else {
                return Ok(());
            };
            return writer.append(
                source_id(frame, "channel:context"),
                Role::System,
                EntryType::Usage,
                json!({
                    "semantics": "context_occupancy",
                    "context_used_tokens": reading.get("usedTokens").cloned().unwrap_or(Value::Null),
                    "context_window_tokens": reading.get("windowTokens").cloned().unwrap_or(Value::Null),
                    "context_used_percent": reading.get("usedPercent").cloned().unwrap_or(Value::Null),
                    "model": reading.get("model").cloned().unwrap_or(Value::Null),
                }),
                true,
            );
        }
        Some("turn") if frame.get("error").is_some_and(|error| !error.is_null()) => {
            return writer.append(
                source_id(frame, "channel:error"),
                Role::System,
                EntryType::Error,
                json!({"code":"harness-error", "retryable":false, "details":frame.get("error").cloned().unwrap_or(Value::Null)}),
                true,
            );
        }
        Some("timeline") => {}
        _ => return Ok(()),
    }
    let event = frame
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let payload = frame.get("payload").unwrap_or(&Value::Null);
    if event == "tool_call" {
        let call = payload
            .get("toolCallId")
            .or_else(|| payload.get("tool_call_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return writer.append(format!("channel:{call}:call"), Role::Assistant, EntryType::ToolCall, json!({"call_id": call, "name": payload.get("toolName").or_else(|| payload.get("tool_name")).and_then(Value::as_str).unwrap_or("unknown"), "arguments": payload.get("input").cloned().unwrap_or(Value::Null)}), true);
    }
    if event == "tool_result" {
        let call = payload
            .get("toolCallId")
            .or_else(|| payload.get("tool_call_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return writer.append(format!("channel:{call}:result"), Role::Tool, EntryType::ToolResult, json!({"call_id": call, "status": if payload.get("isError").and_then(Value::as_bool) == Some(true) {"error"} else {"success"}, "media_type": "application/json", "content": payload.get("content").or_else(|| payload.get("result")).cloned().unwrap_or(Value::Null)}), true);
    }
    if event == "message_end" {
        let message = payload.get("message").unwrap_or(payload);
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("tool") => Role::Tool,
            _ => Role::Assistant,
        };
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| source_id(message, "channel:message"));
        writer.append(
            format!("{id}:message"),
            role,
            EntryType::Message,
            json!({"message_id": id}),
            true,
        )?;
        if let Some(text) = item_text(message) {
            writer.append(
                format!("{id}:content"),
                role,
                EntryType::Content,
                json!({"media_type": "text/plain", "text": text}),
                true,
            )?;
        }
        if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
            // pi and omp report disjoint buckets: `input` excludes cache reads and writes.
            let tokens = |keys: &[&str]| {
                keys.iter()
                    .find_map(|key| usage.get(*key).and_then(Value::as_u64))
                    .unwrap_or(0)
            };
            let input = tokens(&["input", "inputTokens"]);
            let output = tokens(&["output", "outputTokens"]);
            let cache_reads = tokens(&["cacheRead"]);
            let cache_writes = tokens(&["cacheWrite"]);
            let sum = input
                .saturating_add(output)
                .saturating_add(cache_reads)
                .saturating_add(cache_writes);
            let mut body = json!({
                "semantics": "response", "driver": writer.driver,
                "model": message.get("model").and_then(Value::as_str).or(writer.model.as_deref()),
                "provider": message.get("provider").and_then(Value::as_str),
                "input_tokens": input, "output_tokens": output,
                "cached_tokens": cache_reads, "cache_write_tokens": cache_writes,
                "total_tokens": usage.get("totalTokens").and_then(Value::as_u64).filter(|total| *total >= sum).unwrap_or(sum),
            });
            // The harness's own price for this response, when its model registry knows one.
            if let Some(cost) = usage
                .get("cost")
                .and_then(Value::as_f64)
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
            {
                body["cost"] = json!(cost);
                body["currency"] = json!("USD");
            }
            writer.append(
                format!("{id}:usage"),
                Role::System,
                EntryType::Usage,
                body,
                true,
            )?;
        }
    }
    Ok(())
}

fn item_text(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            value
                .get("content")
                .and_then(Value::as_array)
                .and_then(|parts| {
                    let joined = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("");
                    (!joined.is_empty()).then_some(joined)
                })
        })
}

fn safe_summary(_value: &Value) -> String {
    "The harness reported an error.".into()
}

fn source_id(value: &Value, prefix: &str) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = hex_digest(&bytes);
    format!("{prefix}:{}", &digest[..24])
}

fn stable_entry_id(driver: &str, incarnation_id: &str, source_id: &str) -> String {
    let digest = hex_digest(format!("{driver}\0{incarnation_id}\0{source_id}").as_bytes());
    format!("timeline-entry/{}", &digest[..32])
}

fn pseudonym(kind: &str, value: &str) -> String {
    let digest = hex_digest(format!("{kind}\0{value}").as_bytes());
    format!("{kind}/{}", &digest[..24])
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn validate_record(record: &Record) -> bool {
    let fields_valid = crate::contracts::schema_matches(&record.schema, SCHEMA)
        && matches!(
            record.driver.as_str(),
            "codex" | "claude" | "pi" | "omp" | "opencode"
        )
        && !record.incarnation_id.is_empty()
        && record.operations.len() <= MAX_OPERATIONS
        && record.operations.iter().all(|operation| {
            matches!(
                operation.operation.as_str(),
                "append" | "replace" | "finalize"
            ) && matches!(
                operation.role.as_str(),
                "system" | "user" | "assistant" | "tool"
            ) && matches!(
                operation.entry_type.as_str(),
                "message"
                    | "content"
                    | "tool_call"
                    | "tool_result"
                    | "status"
                    | "error"
                    | "usage"
                    | "redaction"
                    | "truncation"
            ) && operation.driver == record.driver
                && operation.incarnation_id == record.incarnation_id
                && operation
                    .source_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("source/"))
                && serde_json::to_vec(&operation.body)
                    .is_ok_and(|body| body.len() <= MAX_BODY_BYTES)
        });
    if !fields_valid {
        return false;
    }
    let mut transitions = BTreeMap::<&str, (u64, bool)>::new();
    for operation in &record.operations {
        match operation.operation.as_str() {
            "append"
                if operation.revision == 1
                    && !transitions.contains_key(operation.entry_id.as_str()) =>
            {
                transitions.insert(&operation.entry_id, (1, operation.final_entry));
            }
            "replace" | "finalize" => {
                let Some((revision, final_entry)) =
                    transitions.get_mut(operation.entry_id.as_str())
                else {
                    return false;
                };
                if *final_entry || operation.revision != *revision + 1 {
                    return false;
                }
                *revision = operation.revision;
                *final_entry = operation.final_entry || operation.operation == "finalize";
            }
            _ => return false,
        }
    }
    true
}

/// Normalize by timeline discriminator. Free-form tool data and provider errors are represented by
/// digests and bounded structural summaries; only user/assistant conversation content follows the
/// explicit text-preservation policy below.
fn normalize_body(entry_type: EntryType, source_id: &str, value: Value) -> (Value, u64, u64, u64) {
    let raw_bytes = serde_json::to_vec(&value).unwrap_or_default();
    match entry_type {
        EntryType::Message => (
            json!({"message_id": pseudonym("message", value.get("message_id").and_then(Value::as_str).unwrap_or(source_id))}),
            0,
            omitted_keys(&value, &["message_id", "reply_to"]),
            0,
        ),
        EntryType::Content => {
            let media_type = value
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("text/plain");
            let text = value.get("text").and_then(Value::as_str).unwrap_or("");
            let (text, redacted, truncated) = normalize_conversation_text(text);
            (
                json!({"media_type": media_type, "text": text}),
                redacted,
                omitted_keys(&value, &["media_type", "text", "attachment_id"]),
                truncated,
            )
        }
        EntryType::ToolCall => {
            let raw_call = value
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or(source_id);
            let arguments = value.get("arguments").cloned().unwrap_or(Value::Null);
            let bytes = serde_json::to_vec(&arguments).unwrap_or_default();
            // Keep the placeholder stable, but null represents no withheld payload.
            let redacted_bytes = if arguments.is_null() {
                0
            } else {
                bytes.len() as u64
            };
            (
                json!({
                    "call_id": pseudonym("call", raw_call),
                    "name": safe_tool_name(value.get("name").and_then(Value::as_str)),
                    "arguments": {"redacted": true, "sha256": hex_digest(&bytes), "bytes": bytes.len()}
                }),
                redacted_bytes,
                omitted_keys(&value, &["call_id", "name", "arguments"]),
                0,
            )
        }
        EntryType::ToolResult => {
            let raw_call = value
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or(source_id);
            let content = value.get("content").cloned().unwrap_or(Value::Null);
            let bytes = serde_json::to_vec(&content).unwrap_or_default();
            let redacted_bytes = if content.is_null() {
                0
            } else {
                bytes.len() as u64
            };
            (
                json!({
                    "call_id": pseudonym("call", raw_call),
                    "status": if value.get("status").and_then(Value::as_str) == Some("error") {"error"} else {"success"},
                    "media_type": "application/vnd.st3.redacted+json",
                    "content": {"redacted": true, "sha256": hex_digest(&bytes), "bytes": bytes.len()}
                }),
                redacted_bytes,
                omitted_keys(&value, &["call_id", "status", "media_type", "content"]),
                0,
            )
        }
        EntryType::Status => {
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| {
                    matches!(
                        *status,
                        "queued" | "running" | "waiting" | "completed" | "failed" | "cancelled"
                    )
                })
                .unwrap_or("waiting");
            let detail = value
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| normalize_conversation_text(detail).0);
            let mut body = json!({"status": status});
            if let Some(detail) = detail {
                body["detail"] = Value::String(detail);
            }
            (body, 0, omitted_keys(&value, &["status", "detail"]), 0)
        }
        EntryType::Error => (
            json!({
                "code": safe_error_code(value.get("code").and_then(Value::as_str)),
                "message": "The harness reported an error.",
                "retryable": value.get("retryable").and_then(Value::as_bool).unwrap_or(false),
                "details": {"redacted": true, "sha256": hex_digest(&raw_bytes)}
            }),
            raw_bytes.len() as u64,
            omitted_keys(&value, &["code", "message", "retryable", "details"]),
            0,
        ),
        EntryType::Usage => {
            let mut body = Map::new();
            for key in [
                "semantics",
                "driver",
                "model",
                "provider",
                "account",
                "input_tokens",
                "output_tokens",
                "cached_tokens",
                "cache_write_tokens",
                "cache_write_1h_tokens",
                "reasoning_tokens",
                "turn_id",
                "total_tokens",
                "context_used_tokens",
                "context_window_tokens",
                "context_used_percent",
                "cost",
                "currency",
            ] {
                if let Some(value) = value.get(key).filter(|value| !value.is_null()) {
                    body.insert(key.into(), value.clone());
                }
            }
            let allowed = [
                "semantics",
                "driver",
                "model",
                "provider",
                "account",
                "input_tokens",
                "output_tokens",
                "cached_tokens",
                "cache_write_tokens",
                "cache_write_1h_tokens",
                "reasoning_tokens",
                "turn_id",
                "total_tokens",
                "context_used_tokens",
                "context_window_tokens",
                "context_used_percent",
                "cost",
                "currency",
            ];
            (Value::Object(body), 0, omitted_keys(&value, &allowed), 0)
        }
        EntryType::Redaction => (
            json!({
                "reason": value.get("reason").and_then(Value::as_str).unwrap_or("sensitive-content"),
                "withheld_bytes": value.get("withheld_bytes").and_then(Value::as_u64).unwrap_or(raw_bytes.len() as u64),
                "withheld_items": value.get("withheld_items").and_then(Value::as_u64).unwrap_or(0),
            }),
            0,
            0,
            0,
        ),
        EntryType::Truncation => (
            json!({
                "reason": value.get("reason").and_then(Value::as_str).unwrap_or("producer-bound"),
                "omitted_from_sequence": value.get("omitted_from_sequence").and_then(Value::as_u64).unwrap_or(0),
                "omitted_to_sequence": value.get("omitted_to_sequence").and_then(Value::as_u64).unwrap_or(0),
            }),
            0,
            0,
            0,
        ),
    }
}

fn omitted_keys(value: &Value, allowed: &[&str]) -> u64 {
    value.as_object().map_or(0, |object| {
        object
            .keys()
            .filter(|key| !allowed.contains(&key.as_str()))
            .count() as u64
    })
}

fn normalize_conversation_text(text: &str) -> (String, u64, u64) {
    let original_chars = text.chars().count();
    let bounded = text.chars().take(MAX_STRING_CHARS).collect::<String>();
    let mut redacted_bytes = 0_u64;
    let mut output = String::with_capacity(bounded.len());
    let mut cursor = 0;
    let mut redact_next = false;
    let words = bounded.match_indices(|character: char| !character.is_whitespace());
    for (start, _) in words {
        if start < cursor {
            continue;
        }
        let end = bounded[start..]
            .find(char::is_whitespace)
            .map_or(bounded.len(), |offset| start + offset);
        let word = &bounded[start..end];
        output.push_str(&bounded[cursor..start]);
        let lower = word.to_ascii_lowercase();
        let sensitive = redact_next
            || lower.starts_with("sk-")
            || lower.starts_with("ghp_")
            || word.starts_with("AKIA")
            || lower.starts_with("token=")
            || lower.starts_with("password=")
            || lower.starts_with("authorization=");
        if sensitive {
            redacted_bytes = redacted_bytes.saturating_add(word.len() as u64);
            if let Some((prefix, _)) = word.split_once('=') {
                output.push_str(prefix);
                output.push_str("=[REDACTED]");
            } else {
                output.push_str("[REDACTED]");
            }
            redact_next = false;
        } else {
            output.push_str(word);
            redact_next = lower == "bearer" || lower == "authorization:";
        }
        cursor = end;
    }
    output.push_str(&bounded[cursor..]);
    (
        output,
        redacted_bytes,
        original_chars.saturating_sub(MAX_STRING_CHARS) as u64,
    )
}

fn safe_tool_name(value: Option<&str>) -> String {
    value
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 128
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
        })
        .unwrap_or("unknown")
        .to_owned()
}

fn safe_error_code(value: Option<&str>) -> String {
    value
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 128
                && code
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        })
        .unwrap_or("harness-error")
        .to_owned()
}

fn compact_to_bounds(record: &mut Record) -> Result<Vec<u8>> {
    const NOTICE_RESERVE_BYTES: u64 = 2_048;
    let mut omitted = None::<(u64, u64)>;
    loop {
        let bytes = serde_json::to_vec(record)?;
        let within_bound = record.operations.len()
            <= MAX_OPERATIONS.saturating_sub(usize::from(omitted.is_some()))
            && bytes.len() as u64
                <= MAX_RECORD_BYTES.saturating_sub(if omitted.is_some() {
                    NOTICE_RESERVE_BYTES
                } else {
                    0
                });
        if within_bound {
            if let Some((from, to)) = omitted {
                push_notice(
                    record,
                    &record.driver.clone(),
                    &record.incarnation_id.clone(),
                    &format!("source/retention-gap:{from}:{to}"),
                    EntryType::Truncation,
                    json!({
                        "reason": "producer-retention",
                        "omitted_from_sequence": from,
                        "omitted_to_sequence": to,
                    }),
                    now_ms(),
                );
                let bytes = serde_json::to_vec(record)?;
                anyhow::ensure!(
                    record.operations.len() <= MAX_OPERATIONS
                        && bytes.len() as u64 <= MAX_RECORD_BYTES,
                    "the harness timeline retention notice exceeds its byte bound"
                );
                return Ok(bytes);
            }
            return Ok(bytes);
        }
        let Some(entry_id) = record.operations.iter().find_map(|candidate| {
            let last = record
                .operations
                .iter()
                .rev()
                .find(|operation| operation.entry_id == candidate.entry_id)?;
            last.final_entry.then(|| candidate.entry_id.clone())
        }) else {
            anyhow::bail!("the harness timeline exceeds its byte bound with only open entries");
        };
        let mut from = u64::MAX;
        let mut to = 0;
        for operation in record
            .operations
            .iter()
            .filter(|operation| operation.entry_id == entry_id)
        {
            from = from.min(
                operation
                    .body
                    .get("omitted_from_sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(operation.sequence),
            );
            to = to.max(
                operation
                    .body
                    .get("omitted_to_sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(operation.sequence),
            );
        }
        omitted = Some(match omitted {
            Some((prior_from, prior_to)) => (prior_from.min(from), prior_to.max(to)),
            None => (from, to),
        });
        // Remove the complete transition chain as a unit. Prefix draining could leave a
        // replace/finalize without its append, which the server must reject.
        record
            .operations
            .retain(|operation| operation.entry_id != entry_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tool_payloads_keep_outbox_operations_without_redaction_notices() {
        let temporary = tempfile::tempdir().unwrap();
        crate::harness_events::enable(temporary.path(), "runtime-current").unwrap();
        crate::harness_state::claim(
            temporary.path(),
            "example/seat",
            "codex",
            "provider-current",
        )
        .unwrap();
        let mut writer = Writer::new(temporary.path(), "codex", "provider-current");
        for (source, kind, body) in [
            ("call-absent", EntryType::ToolCall, json!({"name":"exec"})),
            (
                "call-null",
                EntryType::ToolCall,
                json!({"name":"exec","arguments":null}),
            ),
            (
                "result-absent",
                EntryType::ToolResult,
                json!({"status":"success"}),
            ),
            (
                "result-null",
                EntryType::ToolResult,
                json!({"status":"success","content":null}),
            ),
        ] {
            writer
                .append(source, Role::Assistant, kind, body.clone(), true)
                .unwrap();
            // Repeating a finalized event still produces no additional operation.
            writer
                .append(source, Role::Assistant, kind, body, true)
                .unwrap();
        }
        drop(writer);
        let events = crate::harness_events::pending(temporary.path(), 100).unwrap();
        let timeline: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "harness-timeline")
            .collect();
        assert_eq!(timeline.len(), 4);
        for (event, (kind, payload)) in timeline.iter().zip([
            ("tool_call", "arguments"),
            ("tool_call", "arguments"),
            ("tool_result", "content"),
            ("tool_result", "content"),
        ]) {
            assert_eq!(event.payload["entry_type"], kind);
            assert_eq!(event.payload["operation"], "append");
            assert_eq!(event.payload["final_entry"], true);
            assert_eq!(
                event.payload["body"][payload],
                json!({
                    "redacted":true,"sha256":hex_digest(b"null"),"bytes":4
                })
            );
        }
        assert!(!timeline_path(temporary.path()).exists());
    }

    #[test]
    fn real_tool_payloads_and_omitted_fields_still_publish_redaction_notices() {
        let temporary = tempfile::tempdir().unwrap();
        crate::harness_events::enable(temporary.path(), "runtime-current").unwrap();
        crate::harness_state::claim(
            temporary.path(),
            "example/seat",
            "codex",
            "provider-current",
        )
        .unwrap();
        let mut writer = Writer::new(temporary.path(), "codex", "provider-current");
        for (source, kind, body) in [
            (
                "call",
                EntryType::ToolCall,
                json!({"name":"exec","arguments":{"private":"fixture-secret"}}),
            ),
            (
                "result",
                EntryType::ToolResult,
                json!({"content":"fixture-secret"}),
            ),
            (
                "omitted",
                EntryType::ToolCall,
                json!({"arguments":null,"extra":"fixture-secret"}),
            ),
        ] {
            writer
                .append(source, Role::Assistant, kind, body, true)
                .unwrap();
        }
        let events = crate::harness_events::pending(temporary.path(), 100).unwrap();
        let timeline: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "harness-timeline")
            .collect();
        assert_eq!(timeline.len(), 6);
        let notices: Vec<_> = timeline
            .iter()
            .filter(|event| event.payload["entry_type"] == "redaction")
            .collect();
        assert_eq!(notices.len(), 3);
        assert!(
            notices[..2]
                .iter()
                .all(|event| event.payload["body"]["withheld_bytes"].as_u64().unwrap() > 4)
        );
        assert_eq!(
            notices[2].payload["body"],
            json!({
                "reason":"sensitive-content","withheld_bytes":0,"withheld_items":1
            })
        );
        assert!(
            !serde_json::to_string(
                &timeline
                    .iter()
                    .map(|event| &event.payload)
                    .collect::<Vec<_>>()
            )
            .unwrap()
            .contains("fixture-secret")
        );
    }

    #[test]
    fn claude_stop_publishes_bounded_assistant_text_from_only_its_exact_transcript() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let projects = home.join(".claude/projects/workspace");
        fs::create_dir_all(&projects).unwrap();
        let transcript = projects.join("native-current.jsonl");
        fs::write(&transcript, format!("{}\n{}\n{}\n",
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"answer-1","content":[{"type":"text","text":"draft"}]}}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"answer-1","content":[{"type":"text","text":"Final answer"}]}}),
            json!({"type":"assistant","sessionId":"another-session","message":{"id":"foreign","content":[{"type":"text","text":"Foreign answer"}]}}),
        )).unwrap();
        let agent = temporary.path().join("agent");
        let mut writer = Writer::new(&agent, "claude", "inc-current");
        let payload = json!({"session_id":"native-current","transcript_path":transcript});
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        let record = read(&timeline_path(&agent)).unwrap();
        let content = record
            .operations
            .iter()
            .filter(|op| op.entry_type == "content")
            .collect::<Vec<_>>();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].body["text"], "Final answer");
        let wrong = json!({"session_id":"different","transcript_path":transcript});
        assert!(observe_claude_stop_transcript(&mut writer, &wrong, &home).is_err());
    }

    #[test]
    fn claude_stop_records_each_final_response_usage_once_with_its_turn() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let projects = home.join(".claude/projects/workspace");
        fs::create_dir_all(&projects).unwrap();
        let transcript = projects.join("native-current.jsonl");
        let lines = [
            json!({"type":"user","uuid":"turn-a","sessionId":"native-current"}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"response-a","model":"claude-opus-example","usage":{"input_tokens":7,"output_tokens":2,"cache_creation_input_tokens":11,"cache_read_input_tokens":13}}}),
            json!({"type":"user","uuid":"tool-result-a","sessionId":"native-current","message":{"content":[{"type":"tool_result","tool_use_id":"tool-a","content":"done"}]}}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"response-a","model":"claude-opus-example","usage":{"input_tokens":7,"output_tokens":5,"cache_creation_input_tokens":11,"cache_read_input_tokens":13}}}),
            json!({"type":"user","uuid":"turn-b","sessionId":"native-current"}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"response-b","model":"claude-opus-example","usage":{"input_tokens":3,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":17}}}),
        ];
        fs::write(
            &transcript,
            lines[..4]
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let agent = temporary.path().join("agent");
        let mut writer = Writer::new(&agent, "claude", "inc-current");
        let payload = json!({"session_id":"native-current","transcript_path":transcript});
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        fs::write(
            &transcript,
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        let record = read(&timeline_path(&agent)).unwrap();
        let usage = record
            .operations
            .iter()
            .filter(|op| op.entry_type == "usage")
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].body["turn_id"], "turn-a");
        assert_eq!(usage[0].body["output_tokens"], 5);
        assert_eq!(usage[0].body["cache_write_tokens"], 11);
        assert_eq!(usage[0].body["cached_tokens"], 13);
        assert_eq!(usage[0].body["total_tokens"], 36);
        assert_eq!(usage[1].body["turn_id"], "turn-b");
    }

    #[test]
    fn claude_stop_cursor_keeps_usage_before_large_tool_output() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let projects = home.join(".claude/projects/workspace");
        fs::create_dir_all(&projects).unwrap();
        let transcript = projects.join("native-current.jsonl");
        let lines = [
            json!({"type":"user","uuid":"turn-a","sessionId":"native-current"}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"response-a","model":"claude-example","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":5}}}),
            json!({"type":"user","uuid":"tool-a","sessionId":"native-current","message":{"content":[{"type":"tool_result","content":"x".repeat(3 * 1024 * 1024)}]}}),
            json!({"type":"assistant","sessionId":"native-current","message":{"id":"response-b","model":"claude-example","usage":{"input_tokens":7,"output_tokens":11,"cache_read_input_tokens":13}}}),
        ];
        fs::write(
            &transcript,
            format!(
                "{}\n",
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .unwrap();
        let agent = temporary.path().join("agent");
        let mut writer = Writer::new(&agent, "claude", "inc-current");
        let payload = json!({"session_id":"native-current","transcript_path":transcript});
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        let record = read(&timeline_path(&agent)).unwrap();
        let usage = record
            .operations
            .iter()
            .filter(|op| op.entry_type == "usage")
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].body["turn_id"], "turn-a");
        assert_eq!(usage[1].body["turn_id"], "turn-a");
        assert_eq!(usage[0].body["total_tokens"], 10);
        assert_eq!(usage[1].body["total_tokens"], 31);
    }

    #[test]
    fn codex_keeps_multiple_responses_in_one_turn() {
        let temporary = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(temporary.path(), "codex", "inc-current")
            .with_model(Some("gpt-example".into()));
        for (last, total) in [(10, 10), (20, 30)] {
            observe_codex(&mut writer, &json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-a","turnId":"turn-a","tokenUsage":{"last":{"inputTokens":last,"outputTokens":0,"cachedInputTokens":0,"totalTokens":last},"total":{"totalTokens":total}}}}), "thread-a").unwrap();
        }
        let record = read(&timeline_path(temporary.path())).unwrap();
        let usage = record
            .operations
            .iter()
            .filter(|op| op.entry_type == "usage")
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].body["model"], "gpt-example");
        assert_eq!(
            usage
                .iter()
                .map(|op| op.body["total_tokens"].as_u64().unwrap())
                .sum::<u64>(),
            30
        );
    }

    #[test]
    fn unchanged_channel_state_does_not_crowd_conversation_out_of_a_bounded_page() {
        let temporary = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(temporary.path(), "claude", "inc-current");
        for index in 0..200 {
            observe_channel_frame(
                &mut writer,
                &json!({"type":"state","state":"active","id":index}),
            )
            .unwrap();
        }
        let record = read(&timeline_path(temporary.path())).unwrap();
        assert_eq!(record.operations.len(), 1);
        observe_channel_frame(
            &mut writer,
            &json!({"type":"state","state":"idle","id":201}),
        )
        .unwrap();
        observe_channel_frame(
            &mut writer,
            &json!({"type":"state","state":"active","id":202}),
        )
        .unwrap();
        let record = read(&timeline_path(temporary.path())).unwrap();
        assert_eq!(record.operations.len(), 3);
        assert_eq!(record.operations[0].body["status"], "running");
        assert_eq!(record.operations[1].body["status"], "waiting");
        assert_eq!(record.operations[2].body["status"], "running");
    }

    #[test]
    fn operations_are_stable_revisable_bounded_and_redacted() {
        let temporary = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(temporary.path(), "codex", "inc-1");
        writer
            .append(
                "stream-1",
                Role::Assistant,
                EntryType::Content,
                json!({"media_type":"text/markdown", "text":"# Draft\n\n  indented  text\nBearer plaintext\n```sh\ntrue\n```"}),
                false,
            )
            .unwrap();
        drop(writer);
        let mut restarted = Writer::new(temporary.path(), "codex", "inc-1");
        restarted
            .append(
                "stream-1",
                Role::Assistant,
                EntryType::Content,
                json!({"media_type":"text/markdown", "text":"# Done\n\n  indented  text\nBearer plaintext\n```sh\ntrue\n```"}),
                true,
            )
            .unwrap();
        restarted
            .append(
                "stream-1",
                Role::Assistant,
                EntryType::Content,
                json!({"media_type":"text/plain", "text":"late"}),
                true,
            )
            .unwrap();
        let record = read(&timeline_path(temporary.path())).unwrap();
        let content = record
            .operations
            .iter()
            .filter(|op| op.entry_type == "content")
            .collect::<Vec<_>>();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0].operation, "append");
        assert_eq!(content[1].operation, "finalize");
        assert_eq!(content[0].entry_id, content[1].entry_id);
        assert_eq!(content[0].sequence, content[1].sequence);
        assert_eq!(content[1].revision, 2);
        assert_eq!(
            content[1].body["text"],
            "# Done\n\n  indented  text\nBearer [REDACTED]\n```sh\ntrue\n```"
        );
        assert!(
            record
                .operations
                .iter()
                .any(|op| op.entry_type == "redaction")
        );
        let bytes = fs::read(timeline_path(temporary.path())).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("plaintext"));
        assert!(!String::from_utf8_lossy(&bytes).contains("stream-1"));
    }

    #[test]
    fn codex_claude_pi_and_omp_shapes_normalize_without_raw_tool_data() {
        let temporary = tempfile::tempdir().unwrap();
        let fixture = |name: &str| -> Value {
            serde_json::from_str(match name {
                "codex" => {
                    include_str!("../tests/fixtures/harness-timeline/codex-0.146-redacted.json")
                }
                "claude" => {
                    include_str!("../tests/fixtures/harness-timeline/claude-hooks-redacted.json")
                }
                "pi" => include_str!("../tests/fixtures/harness-timeline/pi-0.84.2-redacted.json"),
                "omp" => {
                    include_str!("../tests/fixtures/harness-timeline/omp-18.1.7-redacted.json")
                }
                _ => unreachable!(),
            })
            .unwrap()
        };

        let codex_dir = temporary.path().join("codex");
        let mut codex = Writer::new(&codex_dir, "codex", "codex-inc");
        let codex_fixture = fixture("codex");
        assert_eq!(codex_fixture["provenance"]["version"], "0.146.0");
        for event in codex_fixture["events"].as_array().unwrap() {
            observe_codex(
                &mut codex,
                event,
                codex_fixture["thread_id"].as_str().unwrap(),
            )
            .unwrap();
        }
        let codex_record = read(&timeline_path(&codex_dir)).unwrap();
        assert!(
            codex_record
                .operations
                .iter()
                .any(|op| op.entry_type == "tool_result" && op.body["status"] == "success")
        );
        assert!(
            codex_record
                .operations
                .iter()
                .any(|op| op.entry_type == "usage" && op.body["total_tokens"] == 32237)
        );
        assert!(!codex_record.operations.iter().any(|op| {
            op.entry_type == "content"
                && op
                    .body
                    .to_string()
                    .contains("provider-hidden-reasoning-fixture")
        }));
        let codex_bytes = fs::read(timeline_path(&codex_dir)).unwrap();
        assert!(!String::from_utf8_lossy(&codex_bytes).contains("sk-fixture-secret"));
        assert!(!String::from_utf8_lossy(&codex_bytes).contains("item-command-fixture"));

        let claude_dir = temporary.path().join("claude");
        let mut claude = Writer::new(&claude_dir, "claude", "claude-inc");
        let claude_fixture = fixture("claude");
        assert_eq!(claude_fixture["provenance"]["harness"], "claude");
        for event in claude_fixture["events"].as_array().unwrap() {
            observe_claude(
                &mut claude,
                event["event"].as_str().unwrap(),
                &event["payload"],
            )
            .unwrap();
        }
        let claude_record = read(&timeline_path(&claude_dir)).unwrap();
        assert!(
            claude_record
                .operations
                .iter()
                .any(|op| op.entry_type == "tool_result" && op.body["status"] == "success")
        );
        assert!(
            !String::from_utf8_lossy(&fs::read(timeline_path(&claude_dir)).unwrap())
                .contains("sk-fixture-secret")
        );

        let pi_dir = temporary.path().join("pi");
        let mut pi = Writer::new(&pi_dir, "pi", "pi-inc");
        let pi_fixture = fixture("pi");
        assert_eq!(pi_fixture["provenance"]["version"], "0.84.2");
        for frame in pi_fixture["frames"].as_array().unwrap() {
            observe_channel_frame(&mut pi, frame).unwrap();
        }
        let pi_record = read(&timeline_path(&pi_dir)).unwrap();
        assert!(
            pi_record
                .operations
                .iter()
                .any(|op| op.entry_type == "content"
                    && op.body["text"] == "## Result\n\nExact  spacing\ntoken=[REDACTED]")
        );

        let omp_dir = temporary.path().join("omp");
        let mut omp = Writer::new(&omp_dir, "omp", "omp-inc");
        let omp_fixture = fixture("omp");
        assert_eq!(omp_fixture["provenance"]["version"], "18.1.7");
        for frame in omp_fixture["frames"].as_array().unwrap() {
            observe_channel_frame(&mut omp, frame).unwrap();
        }
        let omp_record = read(&timeline_path(&omp_dir)).unwrap();
        assert_eq!(omp_record.driver, "omp");
        assert!(
            omp_record
                .operations
                .iter()
                .any(|operation| operation.entry_type == "tool_call")
        );
        assert!(
            omp_record
                .operations
                .iter()
                .any(|operation| operation.entry_type == "tool_result")
        );
        assert!(
            !String::from_utf8_lossy(&fs::read(timeline_path(&omp_dir)).unwrap())
                .contains("tool-fixture")
        );
    }

    #[test]
    fn byte_bounds_reject_oversize_and_pruning_keeps_complete_transition_chains() {
        let temporary = tempfile::tempdir().unwrap();
        let oversized = timeline_path(temporary.path());
        fs::create_dir_all(temporary.path()).unwrap();
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_RECORD_BYTES + 1).unwrap();
        assert!(read(&oversized).is_none());

        let operation = |entry: u64, revision: u64, operation: &str, final_entry: bool| Operation {
            operation: operation.into(),
            entry_id: format!("timeline-entry/{entry}"),
            sequence: entry,
            revision,
            role: "assistant".into(),
            entry_type: "content".into(),
            final_entry,
            body: json!({"media_type":"text/plain", "text":"bounded"}),
            driver: "codex".into(),
            incarnation_id: "inc".into(),
            observed_at_unix_ms: 1,
            source_id: Some(format!("source/{entry}")),
        };
        let mut operations = vec![
            operation(0, 1, "append", false),
            operation(0, 2, "finalize", true),
        ];
        for entry in 1..MAX_OPERATIONS as u64 {
            operations.push(operation(entry, 1, "append", true));
        }
        let mut record = Record {
            schema: SCHEMA.into(),
            driver: "codex".into(),
            incarnation_id: "inc".into(),
            next_sequence: MAX_OPERATIONS as u64,
            operations,
        };
        let bytes = compact_to_bounds(&mut record).unwrap();
        assert!(bytes.len() as u64 <= MAX_RECORD_BYTES);
        assert!(record.operations.len() <= MAX_OPERATIONS);
        assert!(validate_record(&record));
        assert!(
            !record
                .operations
                .iter()
                .any(|operation| operation.entry_id == "timeline-entry/0")
        );
        let gap = record
            .operations
            .iter()
            .find(|operation| operation.entry_type == "truncation")
            .unwrap();
        assert_eq!(gap.body["omitted_from_sequence"], 0);
        assert_eq!(gap.body["omitted_to_sequence"], 0);
    }

    fn usage_bodies(agent_dir: &Path) -> Vec<Value> {
        read(&timeline_path(agent_dir))
            .unwrap()
            .operations
            .into_iter()
            .filter(|op| op.entry_type == "usage")
            .map(|op| op.body)
            .collect()
    }

    #[test]
    fn codex_usage_records_disjoint_buckets_and_the_paying_account() {
        let temporary = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(temporary.path(), "codex", "inc-current")
            .with_model(Some("gpt-example".into()))
            .with_account(Some("codex/aaaaaaaaaaaaaaaa".into()));
        // Codex counts cached input inside `inputTokens` and reasoning inside `outputTokens`.
        observe_codex(&mut writer, &json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-a","turnId":"turn-a","tokenUsage":{"last":{"inputTokens":1000,"cachedInputTokens":900,"outputTokens":50,"reasoningOutputTokens":20,"totalTokens":1050},"total":{"totalTokens":1050}}}}), "thread-a").unwrap();
        writer.set_account(Some("codex/bbbbbbbbbbbbbbbb".into()));
        observe_codex(&mut writer, &json!({"method":"thread/tokenUsage/updated","params":{"threadId":"thread-a","turnId":"turn-a","tokenUsage":{"last":{"inputTokens":10,"cachedInputTokens":0,"outputTokens":5,"reasoningOutputTokens":0,"totalTokens":15},"total":{"totalTokens":1065}}}}), "thread-a").unwrap();
        let usage = usage_bodies(temporary.path());
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0]["input_tokens"], 100);
        assert_eq!(usage[0]["cached_tokens"], 900);
        assert_eq!(usage[0]["output_tokens"], 50);
        assert_eq!(usage[0]["reasoning_tokens"], 20);
        assert_eq!(usage[0]["total_tokens"], 1050);
        assert_eq!(usage[0]["account"], "codex/aaaaaaaaaaaaaaaa");
        assert_eq!(
            usage[1]["account"], "codex/bbbbbbbbbbbbbbbb",
            "an account change applies to later responses only"
        );
    }

    #[test]
    fn channel_usage_keeps_model_cache_and_the_harness_cost() {
        let temporary = tempfile::tempdir().unwrap();
        let mut writer = Writer::new(temporary.path(), "omp", "inc-current");
        observe_channel_frame(
            &mut writer,
            &json!({"type":"timeline","event":"message_end","payload":{"message":{"id":"response-a","role":"assistant","model":"claude-example","provider":"anthropic","usage":{"input":3,"output":40,"cacheRead":500,"cacheWrite":60,"totalTokens":603,"cost":0.0125}}}}),
        )
        .unwrap();
        // A harness without a price for its model reports no cost, which must stay unknown.
        observe_channel_frame(
            &mut writer,
            &json!({"type":"timeline","event":"message_end","payload":{"message":{"id":"response-b","role":"assistant","model":"local-example","usage":{"input":7,"output":8,"cost":null}}}}),
        )
        .unwrap();
        let usage = usage_bodies(temporary.path());
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0]["model"], "claude-example");
        assert_eq!(usage[0]["provider"], "anthropic");
        assert_eq!(usage[0]["cached_tokens"], 500);
        assert_eq!(usage[0]["cache_write_tokens"], 60);
        assert_eq!(usage[0]["total_tokens"], 603);
        assert_eq!(usage[0]["cost"], 0.0125);
        assert_eq!(usage[0]["currency"], "USD");
        assert_eq!(usage[1]["total_tokens"], 15);
        assert!(usage[1].get("cost").is_none());
        assert!(usage[1].get("account").is_none());
    }

    #[test]
    fn claude_usage_separates_one_hour_cache_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let project = home.join(".claude/projects/example");
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join("native-a.jsonl");
        fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({"type":"assistant","sessionId":"native-a","message":{"id":"response-a","model":"claude-example","usage":{"input_tokens":2,"output_tokens":30,"cache_creation_input_tokens":400,"cache_read_input_tokens":5000,"cache_creation":{"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":300}}}})
            ),
        )
        .unwrap();
        let agent = temporary.path().join("agent");
        let mut writer = Writer::new(&agent, "claude", "inc-current")
            .with_account(Some("claude/cccccccccccccccc".into()));
        observe_claude_stop_transcript(
            &mut writer,
            &json!({"session_id":"native-a","transcript_path":transcript}),
            &home,
        )
        .unwrap();
        let usage = usage_bodies(&agent);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0]["cache_write_tokens"], 400);
        assert_eq!(usage[0]["cache_write_1h_tokens"], 300);
        assert_eq!(usage[0]["total_tokens"], 5432);
        assert_eq!(usage[0]["account"], "claude/cccccccccccccccc");
    }

    #[test]
    fn claude_stop_counts_subagent_responses_once() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let project = home.join(".claude/projects/example");
        let subagents = project.join("native-a/subagents");
        fs::create_dir_all(&subagents).unwrap();
        let transcript = project.join("native-a.jsonl");
        let response = |id: &str, output: u64| json!({"type":"assistant","sessionId":"native-a","isSidechain":true,"message":{"id":id,"model":"claude-example","usage":{"input_tokens":1,"output_tokens":output,"cache_read_input_tokens":100}}});
        fs::write(
            &transcript,
            format!("{}\n", json!({"type":"assistant","sessionId":"native-a","message":{"id":"main-a","model":"claude-example","usage":{"input_tokens":2,"output_tokens":3}}})),
        )
        .unwrap();
        let subagent = subagents.join("agent-a1.jsonl");
        // A streamed response repeats its usage on each content block; a torn tail waits.
        fs::write(
            &subagent,
            format!(
                "{}\n{}\n{{\"type\":\"assis",
                response("sub-a", 10),
                response("sub-a", 10)
            ),
        )
        .unwrap();
        let agent = temporary.path().join("agent");
        let mut writer = Writer::new(&agent, "claude", "inc-current");
        let payload = json!({"session_id":"native-a","transcript_path":transcript});
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        let usage = usage_bodies(&agent);
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[1]["turn_id"], "subagent:a1");
        assert_eq!(usage[1]["total_tokens"], 111);

        let mut file = fs::OpenOptions::new().append(true).open(&subagent).unwrap();
        use std::io::Write as _;
        // The torn line was garbage; the next complete one is a new response.
        write!(file, "x\n{}\n", response("sub-b", 20)).unwrap();
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        observe_claude_stop_transcript(&mut writer, &payload, &home).unwrap();
        let usage = usage_bodies(&agent);
        assert_eq!(usage.len(), 3, "{usage:#?}");
        assert_eq!(usage[2]["total_tokens"], 121);
    }
}
