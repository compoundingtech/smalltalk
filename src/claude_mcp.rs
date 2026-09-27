//! Minimal Claude channel watcher.
//!
//! The inbox is the durable source of truth. This process keeps only an ephemeral set of
//! filenames delivered during its current lifetime; a restart scans the inbox again. The outer
//! Claude session wrapper owns presence because Claude can close this child before the session ends.

use std::collections::HashSet;
use std::io::{self, BufRead, Write as _};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::harness_state::{Activity, BlockedOn, InputBuffer, Observation, Writer};
use crate::message;
use crate::native_channel::{channel_content, write_json};

const POLL: Duration = Duration::from_millis(250);
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
    let mut initialized_writer = observe_initialized
        .then(|| st3_initialized_writer(&agent_dir, identity))
        .flatten();
    let inbox = message::inbox_dir(&agent_dir);
    let (input_tx, input_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut delivered = HashSet::new();
    let mut initialized = false;
    loop {
        match input_rx.recv_timeout(POLL) {
            Ok(line) => {
                let line = line.context("reading Claude MCP input")?;
                if line.trim().is_empty() {
                    continue;
                }
                let request: Value =
                    serde_json::from_str(&line).context("decoding Claude MCP JSON")?;
                match request.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        write_json(&mut stdout, &initialize_response(&request, server_name))?;
                    }
                    Some("notifications/initialized") => {
                        initialized = true;
                        if let Some(writer) = initialized_writer.as_mut() {
                            writer.observe(
                                Observation::new(
                                    Activity::Ready,
                                    BlockedOn::None,
                                    InputBuffer::Unknown,
                                )
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
                                &mut stdout,
                                &json!({"jsonrpc":"2.0","id":id,"result":{field:[]}}),
                            )?;
                        }
                    }
                    Some("ping") => {
                        if let Some(id) = request.get("id") {
                            write_json(&mut stdout, &json!({"jsonrpc":"2.0","id":id,"result":{}}))?;
                        }
                    }
                    _ => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Claude owns this child over stdio. EOF is the session-lifetime
            // boundary, so do not leave a detached watcher behind.
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
                        Some(reference) => crate::ding::st3_notification_text(
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
        }
        stdout.flush()?;
        thread::sleep(POLL);
    }
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
