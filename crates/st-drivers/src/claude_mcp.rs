//! Minimal Claude channel watcher.
//!
//! The inbox is the durable source of truth. This process keeps only an ephemeral set of
//! filenames delivered during its current lifetime; a restart scans the inbox again. The outer
//! Claude session wrapper owns presence because Claude can close this child before the session ends.
//!
//! The st3 channel follows its installed binary. When a deploy replaces it, the channel carries the
//! handshake, the delivered set, and any partial request line into the new image with `execve`, so
//! Claude keeps the same stdio server. It also writes a small presence file its driver reports to
//! the daemon, which is how st tells a live, current delivery path from a stale one.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};
use crate::message;
use crate::native_channel::{channel_content, write_json};
use crate::reexec::{self, StdinChunk};

const POLL: Duration = Duration::from_millis(250);
const PRESENCE_REFRESH: Duration = Duration::from_secs(1);
const REPLACEMENT_CHECK: Duration = Duration::from_secs(1);
const LEGACY_SERVER_NAME: &str = "st2";
const ST3_SERVER_NAME: &str = "st3";

pub fn run(catalog_root: &Path, identity: &str) -> Result<()> {
    run_named(catalog_root, identity, LEGACY_SERVER_NAME, false)
}

/// Run the ST3-owned public channel.
///
/// The protocol body is shared with the legacy channel, but its MCP identity and readiness edge
/// belong to ST3. Claude starting the stdio server is the first positive proof that an interactive
/// session made it past startup dialogs and actually loaded the channel; mere Claude child
/// liveness is not that proof.
pub fn run_st3(catalog_root: &Path, identity: &str) -> Result<()> {
    run_named(catalog_root, identity, ST3_SERVER_NAME, true)
}

fn run_named(
    catalog_root: &Path,
    identity: &str,
    server_name: &'static str,
    observe_initialized: bool,
) -> Result<()> {
    let agent_dir =
        message::resolve_declared_dir(catalog_root, identity, &crate::run::detect_host())?
            .with_context(|| format!("Claude MCP agent '{identity}' is not declared"))?;
    // Only the st3 channel follows a replaced binary: st3 answers the resume probe and owns the
    // daemon a deploy restarts.
    let resumed = if observe_initialized {
        match reexec::resume_path(reexec::CHANNEL_RESUME_ENV) {
            Some(path) => {
                let state = reexec::read_state::<ChannelResume>(&path);
                reexec::unblock_stop_signals();
                Some(state.context("resuming the Claude channel after a binary replacement")?)
            }
            None => None,
        }
    } else {
        None
    };
    let mut watch = observe_initialized
        .then(reexec::ReplacementWatch::for_current_process)
        .flatten();
    let (mut delivered, mut initialized, mut lines) = match resumed {
        Some(state) => (state.delivered, state.initialized, state.lines),
        None => (HashSet::new(), false, reexec::LineBuffer::default()),
    };
    let mut initialized_writer = (observe_initialized && !initialized)
        .then(|| st3_initialized_writer(&agent_dir, identity))
        .flatten();
    let inbox = message::inbox_dir(&agent_dir);
    let (input_tx, input_rx) = mpsc::channel();
    let spawn_reader = |sender: mpsc::Sender<StdinChunk>| {
        reexec::StdinReader::spawn(move |chunk| sender.send(chunk).is_ok())
    };
    let mut reader = Some(spawn_reader(input_tx.clone()));
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut next_presence = Instant::now();
    let mut next_replacement_check = Instant::now() + REPLACEMENT_CHECK;
    loop {
        match input_rx.recv_timeout(POLL) {
            Ok(StdinChunk::Bytes(bytes)) => {
                lines.push(&bytes);
                while let Some(line) = lines.next_line() {
                    handle_request(
                        &line,
                        server_name,
                        &mut stdout,
                        &mut initialized,
                        &mut initialized_writer,
                    )?;
                }
            }
            // Claude owns this child over stdio. EOF is the session-lifetime
            // boundary, so do not leave a detached watcher behind.
            Ok(StdinChunk::Eof) => {
                if let Some(line) = lines.finish() {
                    handle_request(
                        &line,
                        server_name,
                        &mut stdout,
                        &mut initialized,
                        &mut initialized_writer,
                    )?;
                    stdout.flush()?;
                }
                return Ok(());
            }
            Ok(StdinChunk::Failed(error)) => {
                return Err(error).context("reading Claude MCP input");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if initialized {
            for msg in message::list_inbox(&inbox)? {
                if delivered.insert(msg.filename.clone()) {
                    // The marker is intentionally part of the synthetic user prompt. Claude's
                    // `UserPromptSubmit` hook sees that prompt only after the interactive TUI has
                    // promoted the channel notification into a real model turn. The outer st3
                    // driver correlates this exact immutable inbox filename before publishing
                    // `message.delivered`; writing MCP bytes alone is not a receipt.
                    let notice = match crate::ding::st3_message_reference(&msg) {
                        Some(reference) => crate::ding::st3_ping_text(
                            reference,
                            msg.from.as_deref().unwrap_or_default(),
                            msg.subject.as_deref(),
                            &msg.body,
                        ),
                        None => channel_content(msg.subject.as_deref(), &msg.body),
                    };
                    let content = format!("[st3-delivery:{}]\n{notice}", msg.filename);
                    write_json(
                        &mut stdout,
                        &json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{
                            "content": content,
                            "meta":{"from":msg.from,"messageFilename":msg.filename,"threadFilename":msg.in_reply_to.unwrap_or_else(|| msg.filename.clone()),"identity":identity}
                        }}),
                    )?;
                }
            }
            // Forget files the driver archived, so the carried set stays as small as the inbox.
            if delivered.len() > 256 {
                let present = message::list_inbox(&inbox)?
                    .into_iter()
                    .map(|msg| msg.filename)
                    .collect::<HashSet<_>>();
                delivered.retain(|filename| present.contains(filename));
            }
        }
        stdout.flush()?;
        let now = Instant::now();
        if observe_initialized && now >= next_presence {
            let _ = write_presence(&agent_dir);
            next_presence = now + PRESENCE_REFRESH;
        }
        if now >= next_replacement_check
            && let Some(watch) = watch.as_mut()
        {
            next_replacement_check = now + REPLACEMENT_CHECK;
            if let Some(binary) = watch.ready() {
                // Take every byte the reader already consumed, answer every complete request,
                // and carry only a partial frame across the exec.
                if let Some(reader) = reader.take() {
                    reader.stop();
                }
                while let Ok(chunk) = input_rx.try_recv() {
                    match chunk {
                        StdinChunk::Bytes(bytes) => lines.push(&bytes),
                        StdinChunk::Eof => return Ok(()),
                        StdinChunk::Failed(error) => {
                            return Err(error).context("reading Claude MCP input");
                        }
                    }
                }
                while let Some(line) = lines.next_line() {
                    handle_request(
                        &line,
                        server_name,
                        &mut stdout,
                        &mut initialized,
                        &mut initialized_writer,
                    )?;
                }
                stdout.flush()?;
                let state = ChannelResume {
                    initialized,
                    delivered: std::mem::take(&mut delivered),
                    lines: std::mem::take(&mut lines),
                };
                match reexec::write_state(&agent_dir, "claude-channel-resume", &state) {
                    Ok(path) => {
                        let error = reexec::exec(&binary, reexec::CHANNEL_RESUME_ENV, &path, &[]);
                        let _ = std::fs::remove_file(&path);
                        tracing::warn!(
                            "st3 Claude channel: executing {} failed: {error}",
                            binary.display()
                        );
                    }
                    Err(error) => {
                        tracing::warn!("st3 Claude channel: saving resume state failed: {error:#}");
                    }
                }
                // Keep serving from this image and retry the replacement later.
                watch.refuse_current();
                initialized = state.initialized;
                delivered = state.delivered;
                lines = state.lines;
                reader = Some(spawn_reader(input_tx.clone()));
            }
        }
    }
}

/// What a Claude channel hands its next image: the MCP handshake already happened, which inbox
/// files Claude already saw, and any partial request line read from Claude.
#[derive(Serialize, Deserialize)]
struct ChannelResume {
    initialized: bool,
    delivered: HashSet<String>,
    lines: reexec::LineBuffer,
}

fn handle_request(
    line: &str,
    server_name: &str,
    stdout: &mut impl io::Write,
    initialized: &mut bool,
    initialized_writer: &mut Option<Writer>,
) -> Result<()> {
    if line.trim().is_empty() {
        return Ok(());
    }
    let request: Value = serde_json::from_str(line).context("decoding Claude MCP JSON")?;
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            write_json(stdout, &initialize_response(&request, server_name))?;
        }
        Some("notifications/initialized") => {
            *initialized = true;
            if let Some(writer) = initialized_writer.as_mut() {
                writer.observe(
                    Observation::new(Activity::Ready, BlockedOn::None, InputBuffer::Unknown)
                        .with_reason("channelInitialized"),
                )?;
            }
        }
        Some("tools/list") | Some("resources/list") | Some("prompts/list") => {
            if let Some(id) = request.get("id") {
                let field = if request["method"] == "tools/list" {
                    "tools"
                } else if request["method"] == "resources/list" {
                    "resources"
                } else {
                    "prompts"
                };
                write_json(
                    stdout,
                    &json!({"jsonrpc":"2.0","id":id,"result":{field:[]}}),
                )?;
            }
        }
        Some("ping") => {
            if let Some(id) = request.get("id") {
                write_json(stdout, &json!({"jsonrpc":"2.0","id":id,"result":{}}))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The Claude channel's own liveness, read by its driver: which process delivers into Claude,
/// which st binary it runs, and when it last looked at the inbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelPresence {
    pub pid: u32,
    pub image: Option<String>,
    pub at_unix_ms: u64,
}

const PRESENCE_FILE: &str = "channel-presence.json";

fn write_presence(agent_dir: &Path) -> Result<()> {
    let presence = ChannelPresence {
        pid: std::process::id(),
        image: reexec::running_identity().map(|identity| identity.token()),
        at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };
    let path = agent_dir.join(PRESENCE_FILE);
    let staging = agent_dir.join(format!(".{PRESENCE_FILE}.{}", std::process::id()));
    std::fs::write(&staging, serde_json::to_vec(&presence)?)?;
    std::fs::rename(&staging, &path)?;
    Ok(())
}

/// The channel presence the st3 Claude channel last wrote for this agent directory.
pub fn read_presence(agent_dir: &Path) -> Option<ChannelPresence> {
    serde_json::from_slice(&std::fs::read(agent_dir.join(PRESENCE_FILE)).ok()?).ok()
}

fn st3_initialized_writer(agent_dir: &Path, identity: &str) -> Option<Writer> {
    let runtime_id = std::env::var(crate::claude_session::RUNTIME_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let session = std::env::var(crate::claude_session::SESSION_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let seq = std::env::var(crate::claude_session::SESSION_SEQ_ENV)
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(initialized_writer(
        agent_dir, identity, runtime_id, session, seq,
    ))
}

fn initialized_writer(
    agent_dir: &Path,
    identity: &str,
    runtime_id: String,
    session: String,
    seq: u64,
) -> Writer {
    Writer::new(agent_dir, identity, "claude", Some(runtime_id)).with_ownership(session, seq)
}

fn initialize_response(request: &Value, server_name: &str) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    json!({"jsonrpc":"2.0","id":id,"result":{
        "protocolVersion": request.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
        "capabilities":{"tools":{},"experimental":{"claude/channel":{}}},
        "serverInfo":{"name":server_name,"version":env!("CARGO_PKG_VERSION")}
    }})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_st3_channels_report_their_own_public_mcp_identity() {
        let request = json!({"jsonrpc":"2.0","id":7,"method":"initialize"});
        assert_eq!(
            initialize_response(&request, LEGACY_SERVER_NAME)["result"]["serverInfo"]["name"],
            "st2"
        );
        assert_eq!(
            initialize_response(&request, ST3_SERVER_NAME)["result"]["serverInfo"]["name"],
            "st3"
        );
    }

    #[test]
    fn initialized_st3_channel_is_the_positive_readiness_edge() {
        let temp = tempfile::tempdir().unwrap();
        let identity = "fleet.cos.standing.cos";
        let session = "st3-channel-session";
        let seq = crate::harness_state::claim(temp.path(), identity, "claude", session).unwrap();
        let mut writer = initialized_writer(
            temp.path(),
            identity,
            "fleet.cos.standing.cos".into(),
            session.into(),
            seq,
        );
        writer
            .observe(
                Observation::new(Activity::Ready, BlockedOn::None, InputBuffer::Unknown)
                    .with_reason("channelInitialized"),
            )
            .unwrap();

        let observed = crate::harness_state::read(
            &crate::harness_state::harness_state_path(temp.path()),
            None,
        )
        .unwrap();
        assert_eq!(observed.state, Activity::Ready);
        assert_eq!(observed.reason.as_deref(), Some("channelInitialized"));
    }
}
