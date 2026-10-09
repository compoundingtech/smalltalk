//! Seat-scoped st tools for Codex.
//!
//! A Codex seat's shell runs in a sandbox that cannot reach the st daemon socket, and widening the
//! sandbox to allow it means widening it for every command the model runs. This stdio MCP server
//! is the other route: st supplies it per launch (`-c mcp_servers.st.…` on the app-server), Codex
//! starts it as a trusted local child outside the command sandbox, and it runs a short, fixed list
//! of st operations on the seat's behalf.
//!
//! Codex does not review these calls, so the scope is enforced here, not by Codex:
//!
//! - the acting identity is fixed by the launch (`--subject`), never read from a tool argument;
//! - each tool maps to one fixed st command, built as an argument vector and never a shell string;
//! - every value is a single `--flag=value` argument, so no value can introduce another option;
//! - identifiers are checked against their expected shape before they reach st.
//!
//! The tools are the message and mission-work operations `st skill` teaches a seat. Anything that
//! creates or stops seats, publishes missions, or changes configuration is deliberately absent.

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

/// The MCP server name Codex sees; tools appear to the model as `mcp__st__<tool>`.
pub const SERVER_NAME: &str = "st";

const PROTOCOL_FALLBACK: &str = "2025-06-18";
const OUTPUT_LIMIT: usize = 48 * 1024;
const FIELD_LIMIT: usize = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// st variables the bridge needs. Codex starts MCP children with a small default environment, so
/// the launch names these explicitly with `env_vars`.
pub const FORWARDED_ENV: &[&str] = &[
    "ST_AGENT",
    "ST3_BIN",
    "ST3_ENDPOINT",
    "ST3_INCARNATION",
    "ST3_ACCOUNT",
    "ST3_RUN_DIR",
    "ST_MISSION",
    "ST_MISSION_RUN",
    "ST_RUN_GENERATION",
    "ST_WORKSPACE",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

/// Config overrides that register the bridge on a Codex app-server.
///
/// `default_tools_approval_mode = "approve"` is what lets the mutating tools run without a prompt:
/// Codex otherwise asks before every MCP tool that is not marked read-only. That is safe here only
/// because [`tool_command`] already limits what each tool can do. Releases that predate the key
/// ignore it, and their seats are asked instead.
pub fn app_server_overrides(executable: &str, subject: &str) -> Vec<String> {
    let args = [
        "driver".to_string(),
        "codex-bridge".to_string(),
        format!("--subject={subject}"),
    ];
    let env_vars: Vec<String> = FORWARDED_ENV.iter().map(|name| toml_string(name)).collect();
    vec![
        format!("mcp_servers.{SERVER_NAME}.command={}", toml_string(executable)),
        format!(
            "mcp_servers.{SERVER_NAME}.args=[{}]",
            args.iter()
                .map(|argument| toml_string(argument))
                .collect::<Vec<_>>()
                .join(",")
        ),
        format!("mcp_servers.{SERVER_NAME}.env_vars=[{}]", env_vars.join(",")),
        format!("mcp_servers.{SERVER_NAME}.default_tools_approval_mode=\"approve\""),
    ]
}

/// True when the seat's own launch arguments already configure an `st` MCP server, which they win.
pub fn is_authored(authored_args: &[String]) -> bool {
    let prefix = format!("mcp_servers.{SERVER_NAME}.");
    authored_args.iter().any(|argument| {
        let value = argument
            .strip_prefix("--config=")
            .or_else(|| argument.strip_prefix("-c").filter(|rest| !rest.is_empty()))
            .unwrap_or(argument);
        value.starts_with(&prefix)
    })
}

fn toml_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            control if control.is_control() => quoted.push_str(&format!("\\u{:04x}", control as u32)),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[derive(Clone, Copy)]
enum Kind {
    Reference,
    Step,
    Recipient,
    Duration,
    Text,
}

struct Param {
    name: &'static str,
    kind: Kind,
    required: bool,
    help: &'static str,
}

const fn required(name: &'static str, kind: Kind, help: &'static str) -> Param {
    Param { name, kind, required: true, help }
}

const fn optional(name: &'static str, kind: Kind, help: &'static str) -> Param {
    Param { name, kind, required: false, help }
}

struct Tool {
    name: &'static str,
    description: &'static str,
    read_only: bool,
    /// Fixed leading arguments of the st command.
    command: &'static [&'static str],
    /// The flag that carries the seat's identity.
    identity_flag: Option<&'static str>,
    params: &'static [Param],
    /// The parameter passed as the positional argument, if any.
    positional: Option<&'static str>,
    /// Parameters passed as `--<name>=<value>`; all other parameters are rejected at definition.
    flags: &'static [&'static str],
}

const REFERENCE: &str = "A message reference: `message/ID` or the bare hex ID.";
const STEP: &str = "A mission step: `step-run/GENERATION/NAME`.";

const TOOLS: &[Tool] = &[
    Tool {
        name: "st_messages",
        description: "List this seat's st conversations (its mailbox).",
        read_only: true,
        command: &["conversations", "ls"],
        identity_flag: Some("--as"),
        params: &[],
        positional: None,
        flags: &[],
    },
    Tool {
        name: "st_message_read",
        description: "Read one st message in full.",
        read_only: true,
        command: &["conversations", "read"],
        identity_flag: Some("--as"),
        params: &[required("reference", Kind::Reference, REFERENCE)],
        positional: Some("reference"),
        flags: &[],
    },
    Tool {
        name: "st_message_reply",
        description: "Reply in the thread of a message this seat received.",
        read_only: false,
        command: &["conversations", "reply"],
        identity_flag: Some("--from"),
        params: &[
            required("reference", Kind::Reference, REFERENCE),
            required("body", Kind::Text, "The reply text."),
        ],
        positional: Some("reference"),
        flags: &["body"],
    },
    Tool {
        name: "st_message_archive",
        description: "Close a message this seat has dealt with.",
        read_only: false,
        command: &["conversations", "archive"],
        identity_flag: Some("--as"),
        params: &[required("reference", Kind::Reference, REFERENCE)],
        positional: Some("reference"),
        flags: &[],
    },
    Tool {
        name: "st_message_send",
        description: "Start a new st thread with another agent or a person.",
        read_only: false,
        command: &["conversations", "send"],
        identity_flag: Some("--from"),
        params: &[
            required("to", Kind::Recipient, "`agent/PATH` or `person/NAME`."),
            required("body", Kind::Text, "The message text."),
            optional("subject", Kind::Text, "A short subject line."),
        ],
        positional: Some("to"),
        flags: &["body", "subject"],
    },
    Tool {
        name: "st_work_ls",
        description: "List the mission steps available to this seat.",
        read_only: true,
        command: &["work", "ls"],
        identity_flag: Some("--as"),
        params: &[],
        positional: None,
        flags: &[],
    },
    Tool {
        name: "st_work_show",
        description: "Show one mission step: goals, constraints, state and evidence.",
        read_only: true,
        command: &["work", "show"],
        identity_flag: None,
        params: &[required("step", Kind::Step, STEP)],
        positional: Some("step"),
        flags: &[],
    },
    Tool {
        name: "st_work_claim",
        description: "Claim a ready mission step for this seat and print its goals and constraints.",
        read_only: false,
        command: &["work", "claim"],
        identity_flag: Some("--as"),
        params: &[required("step", Kind::Step, STEP)],
        positional: Some("step"),
        flags: &[],
    },
    Tool {
        name: "st_work_progress",
        description: "Record progress on a step this seat has claimed.",
        read_only: false,
        command: &["work", "progress"],
        identity_flag: Some("--as"),
        params: &[
            required("step", Kind::Step, STEP),
            required("summary", Kind::Text, "What has been done so far."),
            optional("evidence", Kind::Text, "A reference that backs the summary."),
        ],
        positional: Some("step"),
        flags: &["summary", "evidence"],
    },
    Tool {
        name: "st_work_complete",
        description: "Complete a claimed step with its result and evidence.",
        read_only: false,
        command: &["work", "complete"],
        identity_flag: Some("--as"),
        params: &[
            required("step", Kind::Step, STEP),
            required("summary", Kind::Text, "The result."),
            optional("evidence", Kind::Text, "A reference that proves the result."),
        ],
        positional: Some("step"),
        flags: &["summary", "evidence"],
    },
    Tool {
        name: "st_work_fail",
        description: "Record that a claimed step failed, and why.",
        read_only: false,
        command: &["work", "fail"],
        identity_flag: Some("--as"),
        params: &[
            required("step", Kind::Step, STEP),
            required("reason", Kind::Text, "Why the step failed."),
            optional("evidence", Kind::Text, "A reference that backs the reason."),
        ],
        positional: Some("step"),
        flags: &["reason", "evidence"],
    },
    Tool {
        name: "st_work_release",
        description: "Return a claimed step unfinished so another worker can take it.",
        read_only: false,
        command: &["work", "release"],
        identity_flag: Some("--as"),
        params: &[
            required("step", Kind::Step, STEP),
            optional("reason", Kind::Text, "Why the step is returned."),
        ],
        positional: Some("step"),
        flags: &["reason"],
    },
    Tool {
        name: "st_work_extend",
        description: "Add time to a claimed step's budget.",
        read_only: false,
        command: &["work", "extend"],
        identity_flag: Some("--as"),
        params: &[
            required("step", Kind::Step, STEP),
            required("by", Kind::Duration, "Time to add, such as 30m or 2h; at most 7d."),
            required("reason", Kind::Text, "Why the step needs more time."),
        ],
        positional: Some("step"),
        flags: &["by", "reason"],
    },
];

fn check(kind: Kind, name: &str, value: &str) -> Result<()> {
    anyhow::ensure!(!value.is_empty(), "`{name}` is empty");
    anyhow::ensure!(value.len() <= FIELD_LIMIT, "`{name}` is longer than {FIELD_LIMIT} bytes");
    anyhow::ensure!(!value.contains('\0'), "`{name}` contains a NUL byte");
    let word = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/_.-@:".contains(&byte))
            && !text.starts_with(['-', '/'])
            && !text.contains("..")
    };
    match kind {
        Kind::Text => {}
        Kind::Reference => {
            let id = value.strip_prefix("message/").unwrap_or(value);
            anyhow::ensure!(
                word(id) && !id.contains('/'),
                "`{name}` is not a message reference like message/ID"
            );
        }
        Kind::Step => {
            anyhow::ensure!(
                value.starts_with("step-run/") && word(value),
                "`{name}` is not a step like step-run/GENERATION/NAME"
            );
        }
        Kind::Recipient => {
            anyhow::ensure!(
                ["agent/", "person/"].iter().any(|prefix| {
                    value.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty() && !rest.ends_with('/'))
                }) && word(value),
                "`{name}` is not an agent/PATH or person/NAME subject"
            );
        }
        Kind::Duration => {
            let digits = value.trim_end_matches(['s', 'm', 'h', 'd']);
            anyhow::ensure!(
                value.len() > digits.len()
                    && value.len() - digits.len() == 1
                    && !digits.is_empty()
                    && digits.bytes().all(|byte| byte.is_ascii_digit()),
                "`{name}` is not a duration like 30m or 2h"
            );
        }
    }
    Ok(())
}

/// Build the st argument vector for one tool call, or say why the call is outside the tool's scope.
pub fn tool_command(subject: &str, tool: &str, arguments: &Value) -> Result<Vec<String>> {
    let spec = TOOLS
        .iter()
        .find(|spec| spec.name == tool)
        .with_context(|| format!("`{tool}` is not an st tool"))?;
    let object = match arguments {
        Value::Null => None,
        Value::Object(object) => Some(object),
        _ => anyhow::bail!("tool arguments must be an object"),
    };
    if let Some(object) = object {
        for key in object.keys() {
            anyhow::ensure!(
                spec.params.iter().any(|param| param.name == key),
                "`{key}` is not an argument of {tool}"
            );
        }
    }
    let mut command: Vec<String> = spec.command.iter().map(|part| (*part).to_string()).collect();
    let mut positional = None;
    let mut flags = Vec::new();
    for param in spec.params {
        let value = match object.and_then(|object| object.get(param.name)) {
            None | Some(Value::Null) => {
                anyhow::ensure!(!param.required, "`{}` is required", param.name);
                continue;
            }
            Some(Value::String(value)) => value.as_str(),
            Some(_) => anyhow::bail!("`{}` must be a string", param.name),
        };
        check(param.kind, param.name, value)?;
        if spec.positional == Some(param.name) {
            positional = Some(value.to_string());
        } else {
            anyhow::ensure!(spec.flags.contains(&param.name), "`{}` has no place in {tool}", param.name);
            flags.push(format!("--{}={value}", param.name));
        }
    }
    if let Some(flag) = spec.identity_flag {
        command.push(format!("{flag}={subject}"));
    }
    command.extend(flags);
    if let Some(positional) = positional {
        // A reference or step never starts with `-`, but `--` keeps that true by construction.
        command.push("--".into());
        command.push(positional);
    }
    Ok(command)
}

fn tool_definition(spec: &Tool) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in spec.params {
        properties.insert(
            param.name.to_string(),
            json!({ "type": "string", "description": param.help }),
        );
        if param.required {
            required.push(param.name);
        }
    }
    json!({
        "name": spec.name,
        "description": spec.description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": {
            "readOnlyHint": spec.read_only,
            "destructiveHint": false,
            "openWorldHint": false,
        },
    })
}

/// Runs one st command and returns its combined output and whether it succeeded.
pub type Runner<'a> = dyn FnMut(&[String]) -> Result<(String, bool)> + 'a;

/// Answer one JSON-RPC message; `None` for notifications, which take no reply.
pub fn handle(subject: &str, message: &Value, run: &mut Runner<'_>) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str)?;
    let reply = |result: Value| id.clone().map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    let failure = |code: i64, text: String| {
        id.clone().map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": text } }))
    };
    match method {
        "initialize" => {
            let version = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_FALLBACK);
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "st", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => reply(json!({})),
        "tools/list" => reply(json!({ "tools": TOOLS.iter().map(tool_definition).collect::<Vec<_>>() })),
        "tools/call" => {
            let name = message.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            let arguments = message.pointer("/params/arguments").cloned().unwrap_or(Value::Null);
            let (text, is_error) = match tool_command(subject, name, &arguments).and_then(|command| run(&command)) {
                Ok((output, true)) => (output, false),
                Ok((output, false)) => (output, true),
                Err(error) => (format!("{error:#}"), true),
            };
            reply(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        _ if id.is_none() => None,
        other => failure(-32601, format!("method not found: {other}")),
    }
}

fn run_st(executable: &str, command: &[String]) -> Result<(String, bool)> {
    use std::io::Read as _;
    let mut child = Command::new(executable)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {executable}"))?;
    // Keep the first OUTPUT_LIMIT bytes of each stream and drain the rest, so a chatty command
    // never blocks on a full pipe.
    let capture = |mut stream: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut stream).take(OUTPUT_LIMIT as u64 + 1).read_to_end(&mut kept);
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
            kept
        })
    };
    let out = capture(Box::new(child.stdout.take().context("st stdout")?));
    let err = capture(Box::new(child.stderr.take().context("st stderr")?));
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("st did not finish within {} seconds", COMMAND_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let error_text = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if !error_text.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(error_text.trim_end());
    }
    if text.len() > OUTPUT_LIMIT {
        let mut end = OUTPUT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[output truncated]");
    }
    Ok((text, status.success()))
}

/// Serve the bridge on stdin/stdout until Codex closes the pipe.
pub fn run(subject: &str, executable: &str) -> Result<()> {
    anyhow::ensure!(
        subject.starts_with("agent/") && subject.len() > "agent/".len(),
        "the Codex bridge acts as one agent seat, not `{subject}`"
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut run_command = |command: &[String]| run_st(executable, command);
    for line in stdin.lock().lines() {
        let line = line.context("reading the Codex request")?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                let reply = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": error.to_string() } });
                writeln!(stdout, "{reply}")?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(reply) = handle(subject, &message, &mut run_command) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEAT: &str = "agent/fleet/fixture-seat";

    fn command(tool: &str, arguments: Value) -> Result<Vec<String>> {
        tool_command(SEAT, tool, &arguments)
    }

    #[test]
    fn every_tool_acts_as_the_launch_seat_and_nobody_else() {
        for spec in TOOLS {
            let mut arguments = serde_json::Map::new();
            for param in spec.params.iter().filter(|param| param.required) {
                let value = match param.kind {
                    Kind::Reference => "message/abc123",
                    Kind::Step => "step-run/gen/work",
                    Kind::Recipient => "agent/other",
                    Kind::Duration => "30m",
                    Kind::Text => "hello",
                };
                arguments.insert(param.name.into(), json!(value));
            }
            let built = command(spec.name, Value::Object(arguments)).unwrap();
            match spec.identity_flag {
                Some(flag) => assert!(built.contains(&format!("{flag}={SEAT}")), "{}: {built:?}", spec.name),
                None => assert!(!built.iter().any(|part| part.contains(SEAT)), "{built:?}"),
            }
            assert!(!built.iter().any(|part| part == "--as" || part == "--from"), "{built:?}");
        }
    }

    #[test]
    fn identity_cannot_be_supplied_or_overridden_by_the_model() {
        for tool in ["st_messages", "st_work_ls", "st_work_claim"] {
            let error = command(tool, json!({ "step": "step-run/g/x", "as": "person/example" })).unwrap_err();
            assert!(error.to_string().contains("not an argument"), "{error}");
        }
        assert!(command("st_message_reply", json!({ "reference": "message/a", "body": "x", "from": "person/example" })).is_err());
    }

    #[test]
    fn only_the_listed_operations_exist() {
        for tool in ["st_agents_stop", "st_apply", "st_missions_start", "st", "agents", "work", ""] {
            assert!(command(tool, json!({})).is_err(), "{tool}");
        }
        let names: Vec<_> = TOOLS.iter().map(|spec| spec.name).collect();
        assert!(!names.iter().any(|name| name.contains("agents") || name.contains("apply") || name.contains("mission")));
    }

    #[test]
    fn values_are_one_argument_each_and_cannot_start_an_option() {
        let built = command("st_message_reply", json!({ "reference": "message/a1", "body": "--as=person/example extra" })).unwrap();
        assert_eq!(
            built,
            [
                "conversations",
                "reply",
                &format!("--from={SEAT}"),
                "--body=--as=person/example extra",
                "--",
                "message/a1"
            ]
        );
        for bad in ["--as=person/example", "-x", "message/a b", "message/../x", "message/a;ls", "a\nb"] {
            assert!(command("st_message_read", json!({ "reference": bad })).is_err(), "{bad:?}");
        }
        for bad in ["step-run/g/x y", "work/x", "-step-run/g/x", "step-run/../x"] {
            assert!(command("st_work_claim", json!({ "step": bad })).is_err(), "{bad:?}");
        }
        for bad in ["nobody", "agent/", "person/example b", "-agent/x", "agent/../x"] {
            assert!(command("st_message_send", json!({ "to": bad, "body": "x" })).is_err(), "{bad:?}");
        }
        for bad in ["", "30", "m", "1w", "30m ", "-5m", "5mm"] {
            assert!(
                command("st_work_extend", json!({ "step": "step-run/g/x", "by": bad, "reason": "r" })).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn required_arguments_and_types_are_enforced() {
        assert!(command("st_work_progress", json!({ "step": "step-run/g/x" })).is_err());
        assert!(command("st_work_progress", json!({ "step": "step-run/g/x", "summary": 3 })).is_err());
        assert!(command("st_work_progress", json!({ "step": "step-run/g/x", "summary": "" })).is_err());
        assert!(command("st_work_progress", json!([])).is_err());
        assert!(command("st_work_progress", json!({ "step": "step-run/g/x", "summary": "a\0b" })).is_err());
    }

    #[test]
    fn work_commands_match_the_cli() {
        let built = command(
            "st_work_complete",
            json!({ "step": "step-run/g/x", "summary": "done", "evidence": "doc/e" }),
        )
        .unwrap();
        assert_eq!(
            built,
            ["work", "complete", &format!("--as={SEAT}"), "--summary=done", "--evidence=doc/e", "--", "step-run/g/x"]
        );
        assert_eq!(command("st_messages", Value::Null).unwrap(), ["conversations", "ls", &format!("--as={SEAT}")]);
    }

    #[test]
    fn read_only_hint_is_set_only_on_tools_that_cannot_write() {
        for spec in TOOLS {
            let definition = tool_definition(spec);
            let hint = definition.pointer("/annotations/readOnlyHint").and_then(Value::as_bool);
            assert_eq!(hint, Some(spec.read_only), "{}", spec.name);
            if spec.read_only {
                assert!(
                    matches!(spec.command, ["conversations", "ls" | "read"] | ["work", "ls" | "show"]),
                    "{} claims to be read-only but runs {:?}",
                    spec.name,
                    spec.command
                );
            }
        }
    }

    #[test]
    fn protocol_flow_lists_tools_and_runs_one() {
        let mut seen = Vec::new();
        let mut run = |command: &[String]| {
            seen.push(command.to_vec());
            Ok(("ok\n".to_string(), true))
        };
        let initialized = handle(SEAT, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } }), &mut run).unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(SEAT, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &mut run).is_none());
        let listed = handle(SEAT, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }), &mut run).unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), TOOLS.len());
        let called = handle(
            SEAT,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "st_work_show", "arguments": { "step": "step-run/g/x" } } }),
            &mut run,
        )
        .unwrap();
        assert_eq!(called["result"]["isError"], false);
        assert_eq!(called["result"]["content"][0]["text"], "ok\n");
        let refused = handle(
            SEAT,
            &json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "st_work_show", "arguments": { "step": "--help" } } }),
            &mut run,
        )
        .unwrap();
        assert_eq!(refused["result"]["isError"], true);
        let unknown = handle(SEAT, &json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" }), &mut run).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(seen, [vec!["work".to_string(), "show".into(), "--".into(), "step-run/g/x".into()]]);
    }

    #[test]
    fn a_failing_command_is_a_tool_error_with_its_output() {
        let mut run = |_: &[String]| Ok(("st: not claimed\n".to_string(), false));
        let reply = handle(
            SEAT,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "st_work_ls", "arguments": {} } }),
            &mut run,
        )
        .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert_eq!(reply["result"]["content"][0]["text"], "st: not claimed\n");
    }

    #[test]
    fn launch_overrides_register_the_bridge_with_approval_and_environment() {
        let overrides = app_server_overrides("/opt/st \"3\"/st3", SEAT);
        assert_eq!(overrides[0], r#"mcp_servers.st.command="/opt/st \"3\"/st3""#);
        assert_eq!(
            overrides[1],
            format!(r#"mcp_servers.st.args=["driver","codex-bridge","--subject={SEAT}"]"#)
        );
        assert!(overrides[2].starts_with(r#"mcp_servers.st.env_vars=["ST_AGENT","#));
        assert_eq!(overrides[3], r#"mcp_servers.st.default_tools_approval_mode="approve""#);
    }

    #[test]
    fn an_authored_st_server_is_left_alone() {
        let authored = |parts: &[&str]| parts.iter().map(|part| part.to_string()).collect::<Vec<_>>();
        assert!(is_authored(&authored(&["-c", "mcp_servers.st.command=\"x\""])));
        assert!(is_authored(&authored(&["--config=mcp_servers.st.args=[]"])));
        assert!(is_authored(&authored(&["-cmcp_servers.st.command=\"x\""])));
        assert!(!is_authored(&authored(&["-c", "mcp_servers.other.command=\"x\""])));
        assert!(!is_authored(&authored(&["--approve-for-me"])));
    }
}
