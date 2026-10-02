//! The subagents a seat's harness runs, as its driver sees them on this host.
//!
//! The harness's hooks or events write the ledger as subagents start and stop, and the seat's
//! driver reads it each tick and records what changed as claims on the seat. A subagent leaves
//! `running` only into `ended`, and leaves `ended` only once the driver has recorded its end, so
//! no end is lost between the two. Prompts and transcripts stay on the host: the ledger keeps a
//! transcript path only so the driver can count the subagent's tokens when it ends.
//!
//! Claude reports subagents to its hooks. Codex reports each subagent thread's tasks on the
//! parent thread; a thread given a follow-up task after it completed runs again, and each run is
//! one subagent here, `THREAD` and then `THREAD#2` and on.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::flock::{FileLock, Mode, Open, open};
use crate::fsatomic::{self, Durability, Staging};

pub const LEDGER_FILE: &str = "harness-subagents";
const LOCK_FILE: &str = ".harness-subagents.lock";

/// Ends the driver has not recorded yet. Past this the oldest go first.
const MAX_ENDED: usize = 1_024;
/// Launches waiting for their subagent to start.
const MAX_LAUNCHES: usize = 64;
/// A description is one line of at most this many characters.
const MAX_DESCRIPTION_CHARS: usize = 200;
/// How long a subagent may be missing from the harness's running list before it counts as
/// interrupted. Claude lists background subagents at each Stop; one that finishes as the turn ends
/// reports its own stop a moment later.
pub const UNLISTED_GRACE_MS: u64 = 10_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// The driver incarnation that owns the running subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    /// The harness session they run in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub running: BTreeMap<String, Subagent>,
    /// Ended subagents the driver has not recorded yet, oldest first.
    #[serde(default)]
    pub ended: Vec<Ended>,
    /// Subagent launches seen before their subagent started, oldest first.
    #[serde(default)]
    pub launches: Vec<Launch>,
    /// How many runs each harness subagent thread has had, for a harness whose subagents run
    /// again (Codex).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runs: BTreeMap<String, u32>,
    /// The tokens already recorded for each such thread, so a run counts only its own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counted: BTreeMap<String, Tokens>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub started_at_ms: u64,
    /// The subagent's own transcript on this host, read only to count its tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<PathBuf>,
    /// Since when the harness stopped listing this subagent as running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlisted_since_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    #[serde(flatten)]
    pub subagent: Subagent,
    /// One of `st3_schema::SUBAGENT_OUTCOMES`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub ended_at_ms: u64,
    /// The subagent's tokens when its harness reported them with its end. Without them the
    /// driver counts its transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// The parent's usage does not include this subagent's responses yet, so the driver adds its
    /// tokens there when it counts them. Claude records them itself; Codex does not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parent_usage: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub at_ms: u64,
}

impl Ledger {
    /// Move a running subagent to `ended`. Ending one that is not running does nothing.
    pub fn end(&mut self, id: &str, outcome: &str, reason: Option<String>, at_ms: u64) -> bool {
        let Some(subagent) = self.running.remove(id) else {
            return false;
        };
        self.ended.push(Ended {
            subagent,
            outcome: outcome.into(),
            reason,
            ended_at_ms: at_ms,
            tokens: None,
            parent_usage: false,
        });
        if self.ended.len() > MAX_ENDED {
            let excess = self.ended.len() - MAX_ENDED;
            self.ended.drain(..excess);
        }
        true
    }

    /// End every running subagent.
    pub fn end_all(&mut self, outcome: &str, reason: &str, at_ms: u64) {
        for id in self.running.keys().cloned().collect::<Vec<_>>() {
            self.end(&id, outcome, Some(reason.into()), at_ms);
        }
    }

    /// End the subagents the harness stopped listing as running more than the grace ago.
    pub fn end_unlisted(&mut self, now_ms: u64) {
        let unlisted = self
            .running
            .values()
            .filter(|subagent| {
                subagent
                    .unlisted_since_ms
                    .is_some_and(|since| now_ms.saturating_sub(since) >= UNLISTED_GRACE_MS)
            })
            .map(|subagent| subagent.id.clone())
            .collect::<Vec<_>>();
        for id in unlisted {
            self.end(
                &id,
                "interrupted",
                Some("the harness stopped listing it as running".into()),
                now_ms,
            );
        }
    }

    /// Give the ledger to driver incarnation `incarnation`. Subagents an earlier incarnation's
    /// harness started before `started_at_ms` died with that harness.
    pub fn adopt(&mut self, incarnation: &str, started_at_ms: u64) {
        if self.incarnation.as_deref() == Some(incarnation) {
            return;
        }
        let stale = self
            .running
            .values()
            .filter(|subagent| subagent.started_at_ms < started_at_ms)
            .map(|subagent| subagent.id.clone())
            .collect::<Vec<_>>();
        for id in stale {
            self.end(
                &id,
                "harness-exited",
                Some("its harness restarted".into()),
                started_at_ms,
            );
        }
        self.incarnation = Some(incarnation.into());
    }

    fn start(&mut self, subagent: Subagent) {
        if self.running.contains_key(&subagent.id)
            || self
                .ended
                .iter()
                .any(|ended| ended.subagent.id == subagent.id)
        {
            return;
        }
        self.running.insert(subagent.id.clone(), subagent);
    }

    fn change_session(&mut self, session_id: &str, at_ms: u64) {
        if self.session_id.as_deref() == Some(session_id) {
            return;
        }
        let reason = format!("its parent session changed to {session_id}");
        let ending = self
            .running
            .values()
            .filter(|subagent| subagent.session_id.as_deref() != Some(session_id))
            .map(|subagent| subagent.id.clone())
            .collect::<Vec<_>>();
        for id in ending {
            self.end(&id, "session-ended", Some(reason.clone()), at_ms);
        }
        self.session_id = Some(session_id.into());
    }
}

pub fn ledger_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(LEDGER_FILE)
}

/// The ledger as last written. A missing or unreadable ledger is empty.
pub fn read(agent_dir: &Path) -> Ledger {
    fs::read(ledger_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Apply `change` to the ledger under its lock and write it back when it changed.
pub fn update<T>(agent_dir: &Path, change: impl FnOnce(&mut Ledger) -> T) -> Result<T> {
    fs::create_dir_all(agent_dir)?;
    let lock = open(&agent_dir.join(LOCK_FILE), Open::Create)?;
    let _held = FileLock::hold_blocking(lock, Mode::Exclusive)?;
    let before = read(agent_dir);
    let mut ledger = before.clone();
    let result = change(&mut ledger);
    if ledger != before {
        let mut bytes = serde_json::to_vec(&ledger)?;
        bytes.push(b'\n');
        fsatomic::replace(
            &ledger_path(agent_dir),
            &bytes,
            Staging::new(".harness-subagents"),
            Durability::Rename,
        )
        .with_context(|| format!("writing {}", ledger_path(agent_dir).display()))?;
    }
    Ok(result)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn text(payload: &Value, name: &str) -> Option<String> {
    payload
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The first line of a description, bounded.
pub fn one_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut bounded = line.chars().take(MAX_DESCRIPTION_CHARS).collect::<String>();
    if line.chars().count() > MAX_DESCRIPTION_CHARS {
        bounded.push('…');
    }
    Some(bounded)
}

/// Claude's tools that start a subagent.
fn is_claude_subagent_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

/// Record one Claude hook event. Events about subagents start, end, or describe them; Stop lists
/// the background subagents still running; a new or ended session ends its subagents.
pub fn observe_claude(agent_dir: &Path, event: &str, payload: &Value) -> Result<()> {
    if !matches!(
        event,
        "SessionStart" | "SessionEnd" | "PreToolUse" | "SubagentStart" | "SubagentStop" | "Stop"
    ) {
        return Ok(());
    }
    let now = now_ms();
    update(agent_dir, |ledger| {
        apply_claude(ledger, event, payload, now)
    })
}

fn apply_claude(ledger: &mut Ledger, event: &str, payload: &Value, now: u64) {
    let session = text(payload, "session_id");
    let agent_id = text(payload, "agent_id");
    match event {
        "SessionStart" => {
            if let Some(session) = session {
                ledger.change_session(&session, now);
            }
        }
        "SessionEnd" => {
            let reason = text(payload, "reason").map_or_else(
                || "its parent session ended".into(),
                |why| format!("its parent session ended ({why})"),
            );
            ledger.end_all("session-ended", &reason, now);
        }
        "PreToolUse" => {
            if !text(payload, "tool_name").is_some_and(|name| is_claude_subagent_tool(&name)) {
                return;
            }
            let Some(tool_use_id) = text(payload, "tool_use_id") else {
                return;
            };
            let input = payload.get("tool_input").unwrap_or(&Value::Null);
            if ledger
                .launches
                .iter()
                .any(|launch| launch.tool_use_id == tool_use_id)
            {
                return;
            }
            ledger.launches.push(Launch {
                tool_use_id,
                prompt_id: text(payload, "prompt_id"),
                subagent_type: text(input, "subagent_type"),
                description: text(input, "description").and_then(|line| one_line(&line)),
                at_ms: now,
            });
            if ledger.launches.len() > MAX_LAUNCHES {
                let excess = ledger.launches.len() - MAX_LAUNCHES;
                ledger.launches.drain(..excess);
            }
        }
        "SubagentStart" => {
            let Some(id) = agent_id else {
                return;
            };
            if let Some(session) = &session {
                ledger.change_session(session, now);
            }
            let subagent_type = text(payload, "agent_type");
            // Claude names the launching tool call only in a file it writes after this hook, so
            // take the oldest launch of the same type from the same prompt. The driver corrects
            // the description from that file when it can.
            let prompt = text(payload, "prompt_id");
            let launch = ledger
                .launches
                .iter()
                .position(|launch| {
                    launch.prompt_id == prompt
                        && launch.subagent_type.as_deref().unwrap_or("general-purpose")
                            == subagent_type.as_deref().unwrap_or("general-purpose")
                })
                .map(|index| ledger.launches.remove(index));
            ledger.start(Subagent {
                transcript: claude_subagent_transcript(payload, &id),
                id,
                subagent_type,
                description: launch.and_then(|launch| launch.description),
                session_id: session,
                started_at_ms: now,
                unlisted_since_ms: None,
            });
        }
        "SubagentStop" => {
            // A stop for a subagent that never started is Claude's trailing phantom stop.
            let Some(id) = agent_id else {
                return;
            };
            if let Some(subagent) = ledger.running.get_mut(&id)
                && let Some(path) = text(payload, "agent_transcript_path")
            {
                subagent.transcript = Some(PathBuf::from(path));
            }
            ledger.end(&id, "completed", None, now);
        }
        "Stop" => {
            // A subagent's own events carry its ID; only the parent's Stop lists its tasks.
            // Claude builds without the list say nothing about what still runs.
            if agent_id.is_some() {
                return;
            }
            let Some(tasks) = payload.get("background_tasks").and_then(Value::as_array) else {
                return;
            };
            let listed = tasks
                .iter()
                .filter(|task| {
                    task.get("type").and_then(Value::as_str) == Some("subagent")
                        && task.get("status").and_then(Value::as_str) == Some("running")
                })
                .filter_map(|task| text(task, "id"))
                .collect::<BTreeSet<_>>();
            for subagent in ledger.running.values_mut() {
                if listed.contains(&subagent.id) {
                    subagent.unlisted_since_ms = None;
                } else {
                    subagent.unlisted_since_ms.get_or_insert(now);
                }
            }
        }
        _ => {}
    }
}

/// Claude writes a subagent's transcript at `SESSION/subagents/agent-ID.jsonl` beside the
/// parent's `SESSION.jsonl`.
fn claude_subagent_transcript(payload: &Value, id: &str) -> Option<PathBuf> {
    let parent = PathBuf::from(text(payload, "transcript_path")?);
    let session = parent.file_stem()?.to_owned();
    Some(
        parent
            .with_file_name(session)
            .join("subagents")
            .join(format!("agent-{id}.jsonl")),
    )
}

/// The harness thread a subagent run belongs to: `THREAD#N` is run N of `THREAD`.
pub fn subagent_thread(id: &str) -> &str {
    id.split_once('#').map_or(id, |(thread, _)| thread)
}

const MAX_THREADS: usize = 1_024;

/// Record one Codex app-server notification from the parent thread `parent`. Each subagent
/// thread's task is a run: it starts when the thread starts or takes a follow-up task while idle,
/// and ends when the parent hears it completed or was interrupted, or a collab call reports it
/// errored, shut down or gone.
pub fn observe_codex(agent_dir: &Path, message: &Value, parent: &str) -> Result<()> {
    if !matches!(
        message.get("method").and_then(Value::as_str),
        Some("item/started" | "item/completed")
    ) || message.pointer("/params/threadId").and_then(Value::as_str) != Some(parent)
        || !matches!(
            message.pointer("/params/item/type").and_then(Value::as_str),
            Some("subAgentActivity" | "collabAgentToolCall")
        )
    {
        return Ok(());
    }
    let now = now_ms();
    update(agent_dir, |ledger| {
        apply_codex(ledger, message, parent, now)
    })
}

fn apply_codex(ledger: &mut Ledger, message: &Value, parent: &str, now: u64) {
    let item = message.pointer("/params/item").unwrap_or(&Value::Null);
    ledger.change_session(parent, now);
    let run_id = |ledger: &Ledger, thread: &str| match ledger.runs.get(thread) {
        Some(1) | None => thread.to_owned(),
        Some(run) => format!("{thread}#{run}"),
    };
    match item.get("type").and_then(Value::as_str) {
        Some("subAgentActivity") => {
            let Some(thread) = text(item, "agentThreadId") else {
                return;
            };
            let current = run_id(ledger, &thread);
            match item.get("kind").and_then(Value::as_str) {
                Some("started" | "interacted") => {
                    if ledger.running.contains_key(&current) {
                        return;
                    }
                    // A thread seen before and idle now takes a new task.
                    let run = ledger.runs.get(&thread).map_or(1, |run| run + 1);
                    ledger.runs.insert(thread.clone(), run);
                    if ledger.runs.len() > MAX_THREADS {
                        let oldest = ledger.runs.keys().next().cloned();
                        if let Some(oldest) = oldest {
                            ledger.runs.remove(&oldest);
                            ledger.counted.remove(&oldest);
                        }
                    }
                    let id = run_id(ledger, &thread);
                    // Codex names a subagent by its task path, such as `/root/review_docs`.
                    let description = text(item, "agentPath")
                        .and_then(|path| path.rsplit('/').next().map(str::to_owned))
                        .and_then(|name| one_line(&name.replace('_', " ")));
                    ledger.start(Subagent {
                        id,
                        subagent_type: None,
                        description,
                        session_id: Some(parent.into()),
                        started_at_ms: now,
                        transcript: None,
                        unlisted_since_ms: None,
                    });
                }
                Some("completed") => end_codex_run(ledger, &current, "completed", None, now),
                Some("interrupted") => end_codex_run(ledger, &current, "interrupted", None, now),
                _ => {}
            }
        }
        Some("collabAgentToolCall") => {
            // A finished wait or close reports each target's last state; only a terminal one ends
            // the run it names.
            if message.get("method").and_then(Value::as_str) != Some("item/completed") {
                return;
            }
            let Some(states) = item.get("agentsStates").and_then(Value::as_object) else {
                return;
            };
            for (thread, state) in states {
                let (outcome, reason) = match state.get("status").and_then(Value::as_str) {
                    Some("errored") => ("failed", state.get("message").and_then(Value::as_str)),
                    Some("interrupted") => ("interrupted", None),
                    Some("notFound") => ("interrupted", Some("Codex no longer finds it")),
                    Some("completed" | "shutdown") => ("completed", None),
                    _ => continue,
                };
                let current = run_id(ledger, thread);
                let reason = reason.and_then(one_line);
                end_codex_run(ledger, &current, outcome, reason, now);
            }
        }
        _ => {}
    }
}

fn end_codex_run(ledger: &mut Ledger, id: &str, outcome: &str, reason: Option<String>, now: u64) {
    if ledger.end(id, outcome, reason, now)
        && let Some(ended) = ledger.ended.last_mut()
    {
        ended.parent_usage = true;
    }
}

/// A Codex rollout's newest token total and model. Codex counts cached and cache-written input
/// inside its input and reasoning inside its output; these buckets are disjoint, as Claude's are.
pub fn codex_rollout_tokens(rollout: &Path) -> Result<(Tokens, Option<String>)> {
    let mut total = None;
    let mut model = None;
    for line in BufReader::new(fs::File::open(rollout)?).lines() {
        let Ok(value) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if let Some(name) = value.pointer("/payload/model").and_then(Value::as_str) {
                    model = Some(name.to_owned());
                }
            }
            Some("event_msg")
                if value.pointer("/payload/type").and_then(Value::as_str)
                    == Some("token_count") =>
            {
                if let Some(usage) = value.pointer("/payload/info/total_token_usage") {
                    total = Some(usage.clone());
                }
            }
            _ => {}
        }
    }
    let usage = total.unwrap_or(Value::Null);
    let field = |key| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cached = field("cached_input_tokens");
    let cache_writes = field("cache_write_input_tokens");
    let input = field("input_tokens");
    let output = field("output_tokens");
    Ok((
        Tokens {
            input_tokens: input.saturating_sub(cached).saturating_sub(cache_writes),
            output_tokens: output,
            cache_write_tokens: cache_writes,
            cached_tokens: cached,
            total_tokens: field("total_tokens").max(input.saturating_add(output)),
        },
        model,
    ))
}

impl Tokens {
    /// What `self` adds to an earlier total `before` of the same counter.
    pub fn since(self, before: Tokens) -> Tokens {
        Tokens {
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(before.output_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_sub(before.cache_write_tokens),
            cached_tokens: self.cached_tokens.saturating_sub(before.cached_tokens),
            total_tokens: self.total_tokens.saturating_sub(before.total_tokens),
        }
    }
}

/// Add a subagent's tokens to its parent seat's usage, as one response of `driver` in the
/// harness timeline of incarnation `incarnation`. Its source ID names the subagent, so it counts
/// once however often it is recorded.
pub fn record_parent_usage(
    agent_dir: &Path,
    driver: &str,
    incarnation: &str,
    subagent: &str,
    model: Option<&str>,
    tokens: Tokens,
) -> Result<()> {
    // A writer of another incarnation would start the record over, so add only to this one's.
    let record = crate::harness_timeline::read(&crate::harness_timeline::timeline_path(agent_dir));
    anyhow::ensure!(
        record
            .as_ref()
            .is_none_or(|record| record.driver == driver && record.incarnation_id == incarnation),
        "the harness timeline belongs to another incarnation"
    );
    // The parent's newest response names the account that pays for its subagents too.
    let account = record.and_then(|record| {
        record.operations.iter().rev().find_map(|operation| {
            (operation.entry_type == "usage")
                .then(|| operation.body.get("account")?.as_str().map(str::to_owned))
                .flatten()
        })
    });
    let mut writer =
        crate::harness_timeline::Writer::new(agent_dir, driver, incarnation).with_account(account);
    writer.append(
        format!("{driver}:subagent:{subagent}:usage"),
        crate::harness_timeline::Role::System,
        crate::harness_timeline::EntryType::Usage,
        serde_json::json!({
            "semantics": "response", "driver": driver,
            "model": model,
            "turn_id": format!("subagent:{subagent}"),
            "input_tokens": tokens.input_tokens,
            "output_tokens": tokens.output_tokens,
            "cached_tokens": tokens.cached_tokens,
            "cache_write_tokens": tokens.cache_write_tokens,
            "total_tokens": tokens.total_tokens,
        }),
        true,
    )
}

/// A subagent's description and type as Claude recorded them beside its transcript.
pub fn claude_subagent_meta(transcript: &Path) -> Option<(Option<String>, Option<String>)> {
    let meta = transcript.with_extension("meta.json");
    let value: Value = serde_json::from_slice(&fs::read(meta).ok()?).ok()?;
    Some((
        text(&value, "description").and_then(|line| one_line(&line)),
        text(&value, "agentType"),
    ))
}

/// A subagent's own token buckets, as its harness reported them per response.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cached_tokens: u64,
    pub total_tokens: u64,
}

/// The tokens of every response in a Claude subagent transcript beneath `home`'s Claude projects
/// directory. Claude writes a response's usage on each of its lines; the last one counts.
pub fn claude_transcript_tokens(transcript: &Path, home: &Path) -> Result<Tokens> {
    let allowed = home.join(".claude/projects").canonicalize()?;
    let transcript = transcript.canonicalize()?;
    anyhow::ensure!(
        transcript.starts_with(&allowed)
            && transcript
                .extension()
                .is_some_and(|extension| extension == "jsonl"),
        "a subagent transcript must be a JSONL file beneath the Claude projects directory"
    );
    let mut usage = BTreeMap::<String, Tokens>::new();
    for line in BufReader::new(fs::File::open(&transcript)?).lines() {
        let Ok(value) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if value["type"] != "assistant" {
            continue;
        }
        let Some(id) = value
            .pointer("/message/id")
            .or_else(|| value.get("uuid"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(tokens) = value.pointer("/message/usage") else {
            continue;
        };
        let bucket = |key| tokens.get(key).and_then(Value::as_u64).unwrap_or(0);
        let mut response = Tokens {
            input_tokens: bucket("input_tokens"),
            output_tokens: bucket("output_tokens"),
            cache_write_tokens: bucket("cache_creation_input_tokens"),
            cached_tokens: bucket("cache_read_input_tokens"),
            total_tokens: 0,
        };
        response.total_tokens = response
            .input_tokens
            .saturating_add(response.output_tokens)
            .saturating_add(response.cache_write_tokens)
            .saturating_add(response.cached_tokens);
        usage.insert(id.to_owned(), response);
    }
    Ok(usage
        .into_values()
        .fold(Tokens::default(), |sum, response| Tokens {
            input_tokens: sum.input_tokens.saturating_add(response.input_tokens),
            output_tokens: sum.output_tokens.saturating_add(response.output_tokens),
            cache_write_tokens: sum
                .cache_write_tokens
                .saturating_add(response.cache_write_tokens),
            cached_tokens: sum.cached_tokens.saturating_add(response.cached_tokens),
            total_tokens: sum.total_tokens.saturating_add(response.total_tokens),
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(ledger: &mut Ledger, id: &str, prompt: &str, kind: &str, at: u64) {
        started_in(ledger, "s-1", id, prompt, kind, at);
    }

    fn started_in(ledger: &mut Ledger, session: &str, id: &str, prompt: &str, kind: &str, at: u64) {
        apply_claude(
            ledger,
            "SubagentStart",
            &json!({
                "session_id": session, "prompt_id": prompt, "agent_id": id, "agent_type": kind,
                "transcript_path": "/home/someone/.claude/projects/p/s-1.jsonl",
                "hook_event_name": "SubagentStart",
            }),
            at,
        );
    }

    #[test]
    fn a_launch_names_the_subagent_that_starts_from_it() {
        let mut ledger = Ledger::default();
        apply_claude(
            &mut ledger,
            "PreToolUse",
            &json!({
                "session_id": "s-1", "prompt_id": "p-1", "tool_name": "Agent",
                "tool_use_id": "toolu_1",
                "tool_input": {
                    "description": "count files\nand more", "prompt": "secret prompt",
                    "subagent_type": "Explore",
                },
            }),
            10,
        );
        started(&mut ledger, "a1", "p-1", "Explore", 11);
        let subagent = &ledger.running["a1"];
        assert_eq!(subagent.description.as_deref(), Some("count files"));
        assert_eq!(subagent.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(subagent.session_id.as_deref(), Some("s-1"));
        assert_eq!(
            subagent.transcript.as_deref(),
            Some(Path::new(
                "/home/someone/.claude/projects/p/s-1/subagents/agent-a1.jsonl"
            ))
        );
        assert!(ledger.launches.is_empty());
        // The prompt never enters the ledger.
        assert!(!serde_json::to_string(&ledger).unwrap().contains("secret"));
    }

    #[test]
    fn a_stop_ends_only_a_subagent_that_started() {
        let mut ledger = Ledger::default();
        started(&mut ledger, "a1", "p-1", "general-purpose", 11);
        // Claude's phantom stop after a turn names an agent that never ran.
        apply_claude(
            &mut ledger,
            "SubagentStop",
            &json!({"agent_id": "phantom", "agent_type": ""}),
            12,
        );
        assert!(ledger.ended.is_empty());
        apply_claude(
            &mut ledger,
            "SubagentStop",
            &json!({
                "agent_id": "a1", "agent_type": "general-purpose",
                "agent_transcript_path": "/elsewhere/agent-a1.jsonl",
            }),
            13,
        );
        assert!(ledger.running.is_empty());
        assert_eq!(ledger.ended.len(), 1);
        assert_eq!(ledger.ended[0].outcome, "completed");
        assert_eq!(ledger.ended[0].ended_at_ms, 13);
        assert_eq!(
            ledger.ended[0].subagent.transcript.as_deref(),
            Some(Path::new("/elsewhere/agent-a1.jsonl"))
        );
        // A repeated start of an ended subagent does not reopen it.
        started(&mut ledger, "a1", "p-1", "general-purpose", 14);
        assert!(ledger.running.is_empty());
    }

    #[test]
    fn a_subagent_the_parent_stops_listing_is_interrupted_after_the_grace() {
        let mut ledger = Ledger::default();
        started(&mut ledger, "fg", "p-1", "general-purpose", 1);
        started(&mut ledger, "bg", "p-1", "general-purpose", 1);
        let stop = json!({
            "session_id": "s-1",
            "background_tasks": [
                {"id": "bg", "type": "subagent", "status": "running"},
                {"id": "shell", "type": "bash", "status": "running"},
            ],
        });
        apply_claude(&mut ledger, "Stop", &stop, 100);
        // A subagent's own stop-shaped event says nothing about its parent's tasks.
        apply_claude(
            &mut ledger,
            "Stop",
            &json!({"agent_id": "bg", "background_tasks": []}),
            100,
        );
        assert_eq!(ledger.running["fg"].unlisted_since_ms, Some(100));
        assert_eq!(ledger.running["bg"].unlisted_since_ms, None);
        ledger.end_unlisted(100 + UNLISTED_GRACE_MS - 1);
        assert_eq!(ledger.running.len(), 2);
        ledger.end_unlisted(100 + UNLISTED_GRACE_MS);
        assert_eq!(ledger.running.keys().collect::<Vec<_>>(), ["bg"]);
        assert_eq!(ledger.ended[0].outcome, "interrupted");
        // A build without the list leaves every subagent running.
        apply_claude(&mut ledger, "Stop", &json!({"session_id": "s-1"}), 50_000);
        assert_eq!(ledger.running["bg"].unlisted_since_ms, None);
    }

    #[test]
    fn a_new_or_ended_session_ends_its_subagents() {
        let mut ledger = Ledger::default();
        apply_claude(
            &mut ledger,
            "SessionStart",
            &json!({"session_id": "s-1"}),
            1,
        );
        started(&mut ledger, "a1", "p-1", "general-purpose", 2);
        // A compaction restarts the same session.
        apply_claude(
            &mut ledger,
            "SessionStart",
            &json!({"session_id": "s-1"}),
            3,
        );
        assert_eq!(ledger.running.len(), 1);
        apply_claude(
            &mut ledger,
            "SessionStart",
            &json!({"session_id": "s-2"}),
            4,
        );
        assert!(ledger.running.is_empty());
        assert_eq!(ledger.ended[0].outcome, "session-ended");
        started_in(&mut ledger, "s-2", "a2", "p-2", "general-purpose", 5);
        assert_eq!(ledger.running["a2"].session_id.as_deref(), Some("s-2"));
        apply_claude(&mut ledger, "SessionEnd", &json!({"reason": "clear"}), 6);
        assert!(ledger.running.is_empty());
        assert_eq!(
            ledger.ended[1].reason.as_deref(),
            Some("its parent session ended (clear)")
        );
    }

    #[test]
    fn a_new_incarnation_ends_what_the_last_harness_started() {
        let mut ledger = Ledger::default();
        started(&mut ledger, "old", "p-1", "general-purpose", 10);
        started(&mut ledger, "new", "p-1", "general-purpose", 30);
        ledger.adopt("inc-2", 20);
        assert_eq!(ledger.running.keys().collect::<Vec<_>>(), ["new"]);
        assert_eq!(ledger.ended[0].subagent.id, "old");
        assert_eq!(ledger.ended[0].outcome, "harness-exited");
        // The same incarnation adopting again (a driver replaced in place) ends nothing.
        ledger.adopt("inc-2", 40);
        assert_eq!(ledger.running.len(), 1);
    }

    #[test]
    fn the_ledger_round_trips_under_its_lock() {
        let directory = tempfile::tempdir().unwrap();
        update(directory.path(), |ledger| {
            apply_claude(
                ledger,
                "SubagentStart",
                &json!({"session_id": "s", "agent_id": "a", "agent_type": "t"}),
                1,
            );
        })
        .unwrap();
        assert_eq!(
            read(directory.path()).running["a"].subagent_type.as_deref(),
            Some("t")
        );
    }

    fn activity(kind: &str, thread: &str) -> Value {
        json!({
            "method": "item/completed",
            "params": {
                "threadId": "parent", "turnId": "turn-1",
                "item": {
                    "type": "subAgentActivity", "id": format!("call-{kind}-{thread}"),
                    "kind": kind, "agentThreadId": thread, "agentPath": "/root/review_docs",
                },
            },
        })
    }

    #[test]
    fn each_codex_task_of_a_subagent_thread_is_one_run() {
        let mut ledger = Ledger::default();
        apply_codex(&mut ledger, &activity("started", "child"), "parent", 1);
        let run = &ledger.running["child"];
        assert_eq!(run.description.as_deref(), Some("review docs"));
        assert_eq!(run.session_id.as_deref(), Some("parent"));
        // A message to a running thread is part of its run.
        apply_codex(&mut ledger, &activity("interacted", "child"), "parent", 2);
        assert_eq!(ledger.running.len(), 1);
        apply_codex(&mut ledger, &activity("completed", "child"), "parent", 3);
        assert!(ledger.running.is_empty());
        assert_eq!(ledger.ended[0].outcome, "completed");
        assert!(
            ledger.ended[0].parent_usage,
            "Codex leaves the parent's usage to st"
        );
        // A follow-up task to the idle thread is its second run.
        apply_codex(&mut ledger, &activity("interacted", "child"), "parent", 4);
        assert!(ledger.running.contains_key("child#2"));
        assert_eq!(subagent_thread("child#2"), "child");
        apply_codex(&mut ledger, &activity("interrupted", "child"), "parent", 5);
        assert_eq!(ledger.ended[1].subagent.id, "child#2");
        assert_eq!(ledger.ended[1].outcome, "interrupted");
    }

    #[test]
    fn a_codex_collab_call_ends_the_runs_it_reports_finished() {
        let mut ledger = Ledger::default();
        for thread in ["ok", "broken", "gone", "busy"] {
            apply_codex(&mut ledger, &activity("started", thread), "parent", 1);
        }
        let wait = |method: &str| {
            json!({
                "method": method,
                "params": {
                    "threadId": "parent",
                    "item": {
                        "type": "collabAgentToolCall", "id": "call-wait", "tool": "wait",
                        "status": "completed", "senderThreadId": "parent",
                        "receiverThreadIds": ["ok", "broken", "gone", "busy"],
                        "agentsStates": {
                            "ok": {"status": "completed"},
                            "broken": {"status": "errored", "message": "model refused\nmore"},
                            "gone": {"status": "notFound"},
                            "busy": {"status": "running"},
                        },
                    },
                },
            })
        };
        apply_codex(&mut ledger, &wait("item/started"), "parent", 2);
        assert_eq!(
            ledger.running.len(),
            4,
            "only a finished call reports states"
        );
        apply_codex(&mut ledger, &wait("item/completed"), "parent", 3);
        let ends = ledger
            .ended
            .iter()
            .map(|ended| (ended.subagent.id.as_str(), ended.outcome.as_str()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            ends,
            BTreeMap::from([
                ("broken", "failed"),
                ("gone", "interrupted"),
                ("ok", "completed")
            ])
        );
        let broken = ledger
            .ended
            .iter()
            .find(|ended| ended.subagent.id == "broken");
        assert_eq!(broken.unwrap().reason.as_deref(), Some("model refused"));
        assert_eq!(ledger.running.keys().collect::<Vec<_>>(), ["busy"]);
        // A new parent thread is a new session.
        apply_codex(
            &mut ledger,
            &json!({"method": "item/completed", "params": {"threadId": "other",
                "item": {"type": "subAgentActivity", "kind": "started",
                    "agentThreadId": "x", "agentPath": "/root/x", "id": "c"}}}),
            "other",
            4,
        );
        assert_eq!(ledger.ended.last().unwrap().outcome, "session-ended");
    }

    #[test]
    fn codex_events_from_another_thread_or_item_change_nothing() {
        let directory = tempfile::tempdir().unwrap();
        observe_codex(
            directory.path(),
            &activity("started", "child"),
            "someone-else",
        )
        .unwrap();
        observe_codex(
            directory.path(),
            &json!({"method": "item/completed", "params": {"threadId": "parent",
                "item": {"type": "agentMessage", "id": "m"}}}),
            "parent",
        )
        .unwrap();
        assert!(!ledger_path(directory.path()).exists());
        observe_codex(directory.path(), &activity("started", "child"), "parent").unwrap();
        assert!(read(directory.path()).running.contains_key("child"));
    }

    #[test]
    fn a_codex_rollout_reports_its_newest_disjoint_total() {
        let directory = tempfile::tempdir().unwrap();
        let rollout = directory.path().join("rollout-child.jsonl");
        let count = |input: u64, cached: u64, output: u64| {
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {
                    "input_tokens": input, "cached_input_tokens": cached,
                    "cache_write_input_tokens": 0, "output_tokens": output,
                    "reasoning_output_tokens": 3, "total_tokens": input + output,
                },
            }}})
            .to_string()
        };
        fs::write(
            &rollout,
            [
                json!({"type": "turn_context", "payload": {"model": "gpt-example"}}).to_string(),
                count(100, 60, 10),
                count(300, 200, 25),
                json!({"type": "event_msg", "payload": {"type": "task_complete"}}).to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        let (total, model) = codex_rollout_tokens(&rollout).unwrap();
        assert_eq!(model.as_deref(), Some("gpt-example"));
        assert_eq!(
            total,
            Tokens {
                input_tokens: 100,
                output_tokens: 25,
                cache_write_tokens: 0,
                cached_tokens: 200,
                total_tokens: 325,
            }
        );
        let earlier = Tokens {
            input_tokens: 40,
            output_tokens: 10,
            cache_write_tokens: 0,
            cached_tokens: 60,
            total_tokens: 110,
        };
        assert_eq!(total.since(earlier).total_tokens, 215);
    }

    #[test]
    fn a_subagents_usage_joins_only_its_own_incarnations_timeline() {
        let directory = tempfile::tempdir().unwrap();
        let mut parent = crate::harness_timeline::Writer::new(directory.path(), "codex", "inc-1")
            .with_account(Some("codex/aaaaaaaaaaaaaaaa".into()));
        parent
            .append(
                "codex:usage:turn-1:5",
                crate::harness_timeline::Role::System,
                crate::harness_timeline::EntryType::Usage,
                json!({"semantics": "response", "model": "gpt-example", "total_tokens": 5}),
                true,
            )
            .unwrap();
        let tokens = Tokens {
            input_tokens: 1,
            output_tokens: 2,
            cache_write_tokens: 0,
            cached_tokens: 3,
            total_tokens: 6,
        };
        assert!(
            record_parent_usage(directory.path(), "codex", "inc-2", "child", None, tokens).is_err()
        );
        record_parent_usage(
            directory.path(),
            "codex",
            "inc-1",
            "child",
            Some("gpt-example"),
            tokens,
        )
        .unwrap();
        // Recording it again adds nothing.
        record_parent_usage(directory.path(), "codex", "inc-1", "child", None, tokens).unwrap();
        let record = crate::harness_timeline::read(&crate::harness_timeline::timeline_path(
            directory.path(),
        ))
        .unwrap();
        let usage = record
            .operations
            .iter()
            .filter(|operation| operation.entry_type == "usage")
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[1].body["turn_id"], "subagent:child");
        assert_eq!(usage[1].body["total_tokens"], 6);
        assert_eq!(usage[1].body["account"], "codex/aaaaaaaaaaaaaaaa");
    }

    #[test]
    fn tokens_count_each_response_once() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/projects/p/s-1/subagents");
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("agent-a1.jsonl");
        let line = |id: &str, output: u64| {
            json!({
                "type": "assistant",
                "message": {"id": id, "usage": {
                    "input_tokens": 10, "output_tokens": output,
                    "cache_creation_input_tokens": 100, "cache_read_input_tokens": 1000,
                }},
            })
            .to_string()
        };
        fs::write(
            &transcript,
            [
                line("m1", 3),
                line("m1", 205),
                json!({"type": "user"}).to_string(),
                line("m2", 1),
                "not json".into(),
            ]
            .join("\n")
                + "\n",
        )
        .unwrap();
        assert_eq!(
            claude_transcript_tokens(&transcript, home.path()).unwrap(),
            Tokens {
                input_tokens: 20,
                output_tokens: 206,
                cache_write_tokens: 200,
                cached_tokens: 2000,
                total_tokens: 2426,
            }
        );
        let outside = home.path().join("agent-x.jsonl");
        fs::write(&outside, line("m1", 1)).unwrap();
        assert!(claude_transcript_tokens(&outside, home.path()).is_err());
    }
}
