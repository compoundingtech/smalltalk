//! Minimal Claude channel watcher.
//!
//! The inbox is the durable source of truth. This process keeps only an ephemeral set of
//! filenames delivered during its current lifetime; a restart scans the inbox again. The outer
//! Claude session wrapper owns presence because Claude can close this child before the session ends.

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::message;

const POLL: Duration = Duration::from_millis(250);

fn channel_content(subject: Option<&str>, body: &str) -> String {
    match subject.filter(|value| !value.is_empty()) {
        Some(subject) => format!("Subject: {subject}\n\n{body}"),
        None => body.to_owned(),
    }
}

pub fn run(catalog_root: &Path, identity: &str) -> Result<()> {
    let agent_dir = message::resolve_declared_dir(catalog_root, identity, &crate::run::detect_host())?
        .with_context(|| format!("Claude MCP agent '{identity}' is not declared"))?;
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
                        let id = request.get("id").cloned().unwrap_or(Value::Null);
                        write_json(
                            &mut stdout,
                            &json!({"jsonrpc":"2.0","id":id,"result":{
                                "protocolVersion": request.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                                "capabilities":{"tools":{},"experimental":{"claude/channel":{}}},
                                "serverInfo":{"name":"st2","version":env!("CARGO_PKG_VERSION")}
                            }}),
                        )?;
                    }
                    Some("notifications/initialized") => {
                        initialized = true;
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
                    let content = channel_content(msg.subject.as_deref(), &msg.body);
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

fn write_json(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::channel_content;

    #[test]
    fn channel_content_reuses_subject_and_body_envelope() {
        assert_eq!(
            channel_content(Some("subject"), "body"),
            "Subject: subject\n\nbody"
        );
        assert_eq!(channel_content(None, "body"), "body");
        assert_eq!(channel_content(Some(""), "body"), "body");
    }
}
