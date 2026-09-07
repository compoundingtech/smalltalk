//! Controlled Claude launch with a session-owned presence lease.
//!
//! Claude can close its stdio MCP child after startup. That child cannot prove that the interactive
//! provider still lives. This wrapper launches the provider and refreshes presence while that exact
//! child remains alive. It uses the provider's existing terminal process group. The launch body
//! itself lives in [`crate::provider_session`], which every interactive harness wrapper shares.
//!
//! Observed harness state for Claude has two producers with one owner each: hook invocations
//! (`st2 driver claude-observe`, [`run_observe`]) write turn transitions, and the wrapper's poll
//! loop re-stamps and terminates the record through [`SessionObserver`] without ever overwriting a
//! state a hook wrote in between.

use std::io::{Read as _, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result};

use crate::harness_context::{self, Compaction, CompactionTrigger, Harness, RateLimits, Reading};
use crate::harness_state::{Activity, Ask, BlockedOn, InputBuffer, Observation};
use crate::provider_session::{
    PROVIDER_POLL, STOP, SessionObserver, install_signal_handler, run_provider,
};
use crate::{driver_diagnostic, harness_state, message, status};

/// Run one interactive Claude provider and maintain its presence until it exits.
pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    claude_argv: Vec<String>,
) -> Result<()> {
    let agent_dir =
        message::resolve_declared_dir(catalog_root, &identity, &crate::run::detect_host())?
            .with_context(|| format!("Claude driver agent '{identity}' is not declared"))?;
    anyhow::ensure!(
        !claude_argv.is_empty(),
        "Claude driver '{runtime_id}' has no provider argv"
    );
    let claude_argv = prepare_channel_argv(catalog_root, &identity, claude_argv)?;
    let workspace = std::env::current_dir().context("reading the Claude driver workspace")?;
    crate::pretrust::pretrust_claude(std::slice::from_ref(&workspace))
        .with_context(|| format!("admitting Claude driver workspace {}", workspace.display()))?;
    install_signal_handler();
    let observer = SessionObserver::new(&agent_dir, &identity, "claude", &runtime_id)?;
    // The runtime ID reaches hook subprocesses through the provider environment, so their
    // transitions carry the same pty session the wrapper's records do.
    let env = [
        (RUNTIME_ID_ENV.to_string(), runtime_id.clone()),
        // Hook subprocesses adopt the wrapper's incarnation token, so their transitions are this
        // session's records: the wrapper can re-stamp them, and its terminal record fences them.
        (SESSION_ENV.to_string(), observer.session().to_string()),
        (SESSION_SEQ_ENV.to_string(), observer.seq().to_string()),
    ];
    run_provider(
        "Claude",
        &status::status_path(&agent_dir),
        &claude_argv,
        &env,
        status::STATUS_REFRESH,
        PROVIDER_POLL,
        &STOP,
        Some(&observer),
    )
    .with_context(|| format!("running Claude driver '{runtime_id}'"))
}

fn requires_st2_channel(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == "--channels" && pair[1] == crate::claude_channel::CHANNEL)
}

/// Prefer the approved plugin, but preserve an interactive development path when it is absent.
///
/// The fallback keeps its MCP declaration in provider arguments. It does not write project state.
fn prepare_channel_argv(
    catalog_root: &Path,
    identity: &str,
    argv: Vec<String>,
) -> Result<Vec<String>> {
    if !requires_st2_channel(&argv) {
        return Ok(argv);
    }
    match crate::claude_channel::verify_installed() {
        Ok(()) => Ok(argv),
        Err(error) => {
            eprintln!(
                "warning: the approved st2 Claude channel plugin is unavailable: {error:#}\n\
                 warning: using Claude's interactive development channel; Claude can ask for confirmation\n\
                 warning: run `st2 claude-channel install` for unattended startup"
            );
            let executable = std::env::current_exe()
                .context("resolving the st2 executable for the Claude development channel")?;
            development_channel_argv(argv, &executable, catalog_root, identity)
        }
    }
}

fn development_channel_argv(
    argv: Vec<String>,
    executable: &Path,
    catalog_root: &Path,
    identity: &str,
) -> Result<Vec<String>> {
    let mcp = serde_json::json!({
        "mcpServers": {
            "st2": {
                "type": "stdio",
                "command": executable,
                "args": [
                    "--catalog",
                    catalog_root,
                    "driver",
                    "claude-mcp",
                    "--identity",
                    identity
                ]
            }
        }
    });
    let mut output = Vec::with_capacity(argv.len() + 1);
    let mut index = 0;
    let mut replaced = false;
    while index < argv.len() {
        if !replaced
            && argv[index] == "--channels"
            && argv.get(index + 1).map(String::as_str) == Some(crate::claude_channel::CHANNEL)
        {
            output.extend([
                "--mcp-config".to_string(),
                serde_json::to_string(&mcp)
                    .context("serializing the Claude development channel")?,
                "--dangerously-load-development-channels=server:st2".to_string(),
            ]);
            replaced = true;
            index += 2;
            continue;
        }
        output.push(argv[index].clone());
        index += 1;
    }
    anyhow::ensure!(replaced, "the Claude plugin channel selector is missing");
    Ok(output)
}

/// Apply one Claude hook event (payload on stdin) to the agent's observed-harness-state record.
///
/// Invoked per event by the fail-open `claude-observe.sh` hook, so each invocation is its own
/// short-lived writer; the transition counter continues from disk.
/// The env var carrying the wrapper's runtime/task ID into Claude's hook subprocesses.
pub const RUNTIME_ID_ENV: &str = "ST2_CLAUDE_RUNTIME_ID";
/// The env var carrying the wrapper's session incarnation token into Claude's hook subprocesses.
pub const SESSION_ENV: &str = "ST2_CLAUDE_SESSION";
/// The env var carrying the wrapper's claimed ownership sequence beside the token.
pub const SESSION_SEQ_ENV: &str = "ST2_CLAUDE_SESSION_SEQ";

pub fn run_observe(
    catalog_root: &Path,
    identity: &str,
    runtime_id: Option<&str>,
    event: &str,
) -> Result<()> {
    let agent_dir = message::resolve_declared_dir(catalog_root, identity, &crate::run::detect_host())?
        .with_context(|| format!("Claude driver agent '{identity}' is not declared"))?;
    // Counted only once the invocation has its application target: a hook for an undeclared
    // agent errors out before any state is applied and must not inflate `hook_invocations_total`.
    crate::metrics::record_hook_invocation("claude-observe", event);
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    // The numeric axis is independent of the categorical one and is applied first, because the
    // events that carry a compaction edge say nothing about top-level harness state and would
    // otherwise return below. Fail-open: a context record that cannot be written must never stop
    // a hook the harness is waiting on, and the numbers authorize nothing (HC-A02).
    if let Err(error) = observe_compaction(&agent_dir, identity, event, &payload) {
        tracing::warn!("st2 claude-observe: harness-context compaction write failed: {error:#}");
    }
    // The credential axis is independent of both the numbers and the categorical state, and is
    // applied before the observation guard below for the same reason the compaction write is:
    // an edge that carries no top-level state change must still reach its own record.
    if let Some(edge) = provider_auth_edge(event, &payload) {
        publish_provider_auth(&agent_dir, edge);
    }
    let Some(observation) = observe_hook_event(event, &payload) else {
        return Ok(());
    };
    let mut writer = observe_writer(
        &agent_dir,
        identity,
        runtime_id,
        event,
        &payload,
        std::env::var(SESSION_ENV).ok().filter(|t| !t.is_empty()),
        std::env::var(SESSION_SEQ_ENV)
            .ok()
            .and_then(|seq| seq.parse::<u64>().ok()),
    );
    if event == "SessionStart" {
        // The one event that names a session boundary: even if the new session's first state
        // matches a fresh predecessor record, continuity must not be claimed across the restart.
        writer.interrupt();
    }
    // A late hook finishing after the wrapper reaped Claude must not replace the terminal record
    // with a live state: the wrapper's `ended` carries this same token and is the session's last
    // word. (`false` = suppressed; the hook has nothing else to do with it.)
    writer.observe_unless_ended(observation).map(|_wrote| ())
}

/// Select the ownership a hook write acts under. The wrapper's exported token makes hook writes
/// this session's records (adopted ownership when the claimed sequence travels beside it). A
/// wrapperless seat falls back to Claude's own session_id — and because token-only writers never
/// claim, the SessionStart arm IS that path's session boundary and performs the WRITTEN claim
/// (degrading to token-only with a warning if the claim cannot be written); later hooks of the
/// same session adopt its records by token. What such a seat still lacks is a heartbeat and
/// terminal owner — the documented hooks-only limitation.
#[allow(clippy::too_many_arguments)]
fn observe_writer(
    agent_dir: &Path,
    identity: &str,
    runtime_id: Option<&str>,
    event: &str,
    payload: &serde_json::Value,
    exported_session: Option<String>,
    exported_seq: Option<u64>,
) -> harness_state::Writer {
    let pty_session = runtime_id.unwrap_or(identity).to_string();
    let writer = harness_state::Writer::new(agent_dir, identity, "claude", Some(pty_session));
    if let Some(session) = exported_session {
        return match exported_seq {
            Some(seq) => writer.with_ownership(session, seq),
            None => writer.with_session(session),
        };
    }
    if let Some(token) = wrapperless_token(payload) {
        if event == "SessionStart" {
            // Eligibility and the written takeover are ONE act under the record lock: a
            // hooks-only SessionStart racing a wrapper's startup can no longer steal the
            // sequence between the wrapper's read and its write. Ineligible (a live wrapper or
            // its fresh claim placeholder owns the record) or unwritable both degrade to
            // token-only.
            return match harness_state::claim_wrapperless(agent_dir, identity, "claude", &token) {
                Ok(Some(seq)) => writer.with_ownership(token, seq),
                Ok(None) => writer.with_session(token),
                Err(error) => {
                    tracing::warn!(
                        "st2 claude-observe: observed-state claim failed; degrading to token-only: {error:#}"
                    );
                    writer.with_session(token)
                }
            };
        }
        return writer.with_session(token);
    }
    writer
}

/// Claude's own session id under the wrapperless prefix, for a seat that runs `claude` directly
/// with no session wrapper to export a token.
///
/// Factored out so [`observe_writer`] and [`context_writer`] derive the same token from the same
/// payload field: the hook subprocesses and the status-line tee of one seat must publish ONE
/// incarnation, or a reader could not tell a straggler from a sibling (HC-A03, HC-R15).
fn wrapperless_token(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(|id| format!("{}{id}", harness_state::WRAPPERLESS_PREFIX))
}

/// The Claude harness-context writer for one driver subprocess, under the ownership
/// [`observe_writer`] selects: the wrapper's exported token when a wrapper launched this seat,
/// otherwise Claude's own session id under the wrapperless prefix.
///
/// The claim half of that selection is deliberately absent. `incarnation` on this record is
/// PROVENANCE and is never consulted as a fence (HC-R15), so there is nothing here to claim —
/// which is also why the status-line tee, which is not a hook and has no session boundary, can
/// share the selection without sharing the takeover.
fn context_writer(
    agent_dir: &Path,
    identity: &str,
    payload: &serde_json::Value,
) -> Result<harness_context::Writer> {
    let writer = harness_context::Writer::new(agent_dir, identity, Harness::Claude)?;
    let exported = std::env::var(SESSION_ENV).ok().filter(|t| !t.is_empty());
    Ok(match exported.or_else(|| wrapperless_token(payload)) {
        Some(token) => writer.with_session(token),
        None => writer,
    })
}

/// Apply one Claude compaction edge to the agent's harness-context record (HC-R12).
///
/// Claude publishes THREE edges for one compaction — `PreCompact`, `PostCompact`, and a
/// `SessionStart` carrying `source: "compact"` — and each arrives in its own short-lived hook
/// process with nothing durable passed between them. A counter incrementing on more than one of
/// them would treble-count every compaction, so the dedupe is positional rather than stateful:
///
/// - **`PreCompact` is the sole counting edge.** It is the first, it fires for every compaction
///   including one whose `PostCompact` never arrives (a compaction that ends the session, or a
///   future build that drops the event), and it carries `trigger`. Counting on the FIRST edge is
///   what makes the dedupe stateless — "count on the second only if the first was seen" would
///   need per-compaction memory the record deliberately does not carry (HC-T02).
/// - **`PostCompact` does not count.** It holds the count it finds and advances
///   `lastCompactionMs` from when compaction *started* to when the window was actually emptied.
///   Only if it finds no counted compaction at all — no record, or one whose `PreCompact` write
///   never landed — does it count, because it is then the first evidence st2 has.
/// - **`SessionStart source=compact` is recognized and deliberately inert.** It is the same
///   compaction seen a third time; counting it is exactly the double count HC-R12 forbids.
///
/// The counter is incarnation-scoped: st2 does the counting, and `harness-context` is removed at
/// the relaunch claim (HC-R15), so the count describes this incarnation and not the seat's life.
fn observe_compaction(
    agent_dir: &Path,
    identity: &str,
    event: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    // A subagent's compaction is not the top-level session's, and this record describes the
    // top-level window (the status-line payload carries no subagent window either — DQ-C9). The
    // guard matches `observe_hook_event`'s for the same reason.
    if payload
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return Ok(());
    }
    let trigger = compaction_trigger(payload);
    let edge = match event {
        "PreCompact" => Compaction::new(trigger),
        "PostCompact" => {
            match harness_context::read(&harness_context::harness_context_path(agent_dir)) {
                Some(observed) if observed.compactions > 0 => {
                    Compaction::new(trigger).with_count(observed.compactions)
                }
                _ => Compaction::new(trigger),
            }
        }
        _ => return Ok(()),
    };
    context_writer(agent_dir, identity, payload)?
        .compacted(edge)
        .map(|_landed| ())
}

/// The trigger word a Claude compaction hook carries. 2.1.250 publishes `manual` and `auto` on
/// both `PreCompact` and `PostCompact`; the vocabulary is closed and additive-tolerant, so a word
/// this version does not publish decodes as `unknown` rather than as a definite trigger.
fn compaction_trigger(payload: &serde_json::Value) -> CompactionTrigger {
    match payload.get("trigger").and_then(serde_json::Value::as_str) {
        Some("manual") => CompactionTrigger::Manual,
        Some("auto") => CompactionTrigger::Auto,
        _ => CompactionTrigger::Unknown,
    }
}

/// The Claude Code version this producer's arithmetic was measured against (HC-R13). A bump that
/// moves the numerator, the denominator, or the percent rule must fail the fixture rather than
/// silently publish a differently-meaning number.
pub const STATUSLINE_VERSION: &str = "2.1.250";

/// The environment variable holding the operator's downstream status-line renderer. Checked
/// FIRST, so one agent, a debugging session, or a test can override without editing a file.
pub const STATUSLINE_RENDERER_ENV: &str = "ST_CLAUDE_STATUSLINE_RENDERER";

/// The operator-owned renderer file, relative to `$HOME`. Schema
/// `dotfiles.claude-statusline-renderer.v1`, carrying `{"command": …}` — dotfiles owns the file
/// and its shape, and st2 reads `command` and nothing else (`DQ-C2`, dotfiles PR #2160).
///
/// It is a file rather than a settings key because the settings file st2 wins in is the one st2
/// rewrites: a renderer declared there would be the very thing the merge does not preserve, which
/// is HC-R18's inverse. A user-level file st2 never writes has no such hazard.
const STATUSLINE_RENDERER_FILE: &str = ".claude/statusline-renderer.json";

/// Project one Claude status-line payload onto a harness-context reading (HC-R02, HC-R03).
///
/// The status-line payload is the ONLY Claude channel carrying a window: hook payloads have no
/// token fields at all, and the transcript has per-message `usage` but no window size, so a
/// transcript-only producer would have to invent the denominator from a model table — which
/// cannot tell a 200k tier from a 1M tier for one model id, and is exactly what HC-R02 forbids.
///
/// The numerator is read from `current_usage`, NOT from the sibling `total_input_tokens`. They
/// agree whenever Claude knows its occupancy — the bundle's builder defines the latter as
/// `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` of `current_usage` —
/// but it emits `0` for it precisely when `current_usage` is null, so before the session's first
/// API response the sibling is a zero DERIVED FROM AN ABSENCE. Reading `current_usage` makes the
/// withholding structural: no reading, no numerator (HC-R03).
///
/// `used_percent` is Claude's own integer, already clamped to 0..100 by `QUt` in the bundle, and
/// st2 never recomputes it from the operands (HC-R02). `sessionTotalTokens` is `null`: the
/// payload's `total_*` keys describe the last response, not the session, and calling them
/// cumulative would be the exact confusion HC-R16 names.
pub fn statusline_reading(payload: &serde_json::Value) -> Reading {
    let window = payload.pointer("/context_window");
    let usage = window
        .and_then(|window| window.get("current_usage"))
        .filter(|usage| !usage.is_null());
    Reading {
        used_tokens: usage.map(|usage| {
            [
                "input_tokens",
                "cache_creation_input_tokens",
                "cache_read_input_tokens",
            ]
            .into_iter()
            .filter_map(|key| usage.get(key).and_then(serde_json::Value::as_u64))
            .sum()
        }),
        window_tokens: window
            .and_then(|window| window.get("context_window_size"))
            .and_then(serde_json::Value::as_u64),
        used_percent: window
            .and_then(|window| window.get("used_percentage"))
            .and_then(serde_json::Value::as_f64),
        model: payload
            .pointer("/model/id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        cost_usd: payload
            .pointer("/cost/total_cost_usd")
            .and_then(serde_json::Value::as_f64),
        // The payload's `total_input_tokens`/`total_output_tokens` describe the LAST RESPONSE.
        // Claude publishes no cumulative session total, and a producer accumulating one itself
        // would be maintaining a second running total whose correctness depends on having seen
        // every render — a worse answer than none (HC-R16).
        session_total_tokens: None,
        rate_limits: RateLimits {
            five_hour: payload
                .pointer("/rate_limits/five_hour/used_percentage")
                .and_then(serde_json::Value::as_f64),
            seven_day: payload
                .pointer("/rate_limits/seven_day/used_percentage")
                .and_then(serde_json::Value::as_f64),
        },
    }
}

/// The status-line tee: record the payload, then chain to the operator's own renderer (HC-R18).
///
/// Claude's `statusLine` is a SINGLE slot whose winning declaration replaces the others outright
/// — measured 2026-08-29 against 2.1.250 through a real pty, `.claude/settings.local.json` >
/// `.claude/settings.json` > `~/.claude/settings.json`, with the losing renderer never invoked.
/// Since `.claude/settings.local.json` is exactly the file st2 materializes for a driver-declared
/// seat, an st2 entry that does not chain would silently and unconditionally remove the
/// operator's status line on every managed agent, with no warning.
///
/// Every failure here degrades to an EMPTY status line, and stdout carries the renderer's bytes
/// or nothing at all. The payload is a machine-readable JSON object — session id, transcript
/// path, model block, usage block — so echoing it where a status line belongs paints a wall of
/// JSON across the operator's terminal every five seconds, which is strictly worse for them than
/// a blank line and gives them nothing to act on. The reason to degrade is the same either way;
/// only the human-facing line is at stake, and recording is unaffected by which arm runs.
///
/// Recording is best-effort and only warns; so does each degraded arm. Both warn on stderr,
/// which Claude routes to its debug log rather than to the status line, so the diagnostic names
/// what failed without ever touching the rendered row.
pub fn run_statusline(catalog_root: &Path, identity: &str) -> Result<()> {
    let mut raw = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut raw);
    if let Err(error) = record_statusline(catalog_root, identity, &raw) {
        tracing::warn!("st2 claude-statusline: recording failed; chaining anyway: {error:#}");
    }
    chain_statusline(&raw)
}

fn record_statusline(catalog_root: &Path, identity: &str, raw: &[u8]) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_slice(raw).unwrap_or(serde_json::Value::Null);
    let agent_dir = message::resolve_declared_dir(catalog_root, identity, &crate::run::detect_host())?
        .with_context(|| format!("Claude driver agent '{identity}' is not declared"))?;
    // Deliberately uncounted. The tee builds no telemetry pipeline at all (`DQ-C13`, see
    // `main`), so a `record_hook_invocation` here could never reach a collector — and a metric
    // call that provably cannot record is worse than none: it reads as instrumentation.
    // `06-observability`'s spec already scopes `hook_invocations_total` to `claude-observe` and
    // says other hook surfaces are not instrumented yet, which is exactly this.
    context_writer(&agent_dir, identity, &payload)?
        .observe(statusline_reading(&payload))
        .map(|_landed| ())
}

/// Two sources in strict order, first hit wins, never merged and never both — so the resolution
/// has one answer and an operator debugging their status line has one place to look for it.
/// Neither resolving is the third case, handled by the caller as an empty status line.
fn downstream_renderer() -> Option<String> {
    if let Some(command) = std::env::var(STATUSLINE_RENDERER_ENV)
        .ok()
        .filter(|command| !command.trim().is_empty())
    {
        return Some(command);
    }
    let path = std::path::PathBuf::from(std::env::var_os("HOME")?).join(STATUSLINE_RENDERER_FILE);
    let declaration: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    declaration
        .get("command")
        .and_then(serde_json::Value::as_str)
        .filter(|command| !command.trim().is_empty())
        .map(str::to_string)
}

fn chain_statusline(raw: &[u8]) -> Result<()> {
    let Some(command) = downstream_renderer() else {
        // Both resolution paths named, because "no renderer resolved" is the whole diagnosis and
        // the operator's next move is to set one of exactly these two.
        tracing::warn!(
            "st2 claude-statusline: no downstream renderer resolved from \
             ${STATUSLINE_RENDERER_ENV} or ~/{STATUSLINE_RENDERER_FILE}; \
             rendering an empty status line"
        );
        return Ok(());
    };
    // The renderer is a shell command line, exactly as Claude's own `statusLine.command` is, so
    // it is run the way Claude would run it.
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdin(std::process::Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(
                "st2 claude-statusline: downstream renderer `{command}` could not start: {error}"
            );
            return Ok(());
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(raw);
    }
    let _ = child.wait();
    Ok(())
}

/// Map one Claude hook event to an observation, or `None` when the event says nothing about
/// top-level harness state.
///
/// Claude gives no call identity on the event that enters `blocked` (`PermissionRequest` carries
/// no `tool_use_id`; its `prompt_id` is turn-scoped), so the exit edge is the next
/// `PreToolUse`/`PostToolUse`/`Stop`. Measured 2026-08-23 (Claude Code 2.1.237, DQ-H1): tool
/// execution serializes around an open permission prompt — no hook event fires while a prompt is
/// up, even for a parallel-batched allowlisted call — so that next event is the blocked call's own
/// resolution and the batched false-clear #268 §C predicted cannot occur. The residual limit is
/// denial: "No" ends the turn with zero further events (no Stop, and no PermissionDenied even when
/// registered), so `blocked` stands until the next `UserPromptSubmit`/`SessionStart`.
pub fn observe_hook_event(event: &str, payload: &serde_json::Value) -> Option<Observation> {
    // Any event carrying an agent identity is a subagent's and must never move top-level state:
    // a phantom `SubagentStop` trails every completed turn, 1.5-2.9s after `Stop`. That phantom
    // populating `agent_id` is an undocumented emergent property — if a future Claude build omits
    // it, subagent completions read as top-level activity again and nothing here would catch it.
    if payload
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        return None;
    }
    match event {
        "SessionStart" => Some(
            Observation::new(Activity::Idle, BlockedOn::None, InputBuffer::Unknown)
                .with_reason("sessionStart"),
        ),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Some(Observation::new(
            Activity::Active,
            BlockedOn::None,
            InputBuffer::Unknown,
        )),
        "Stop" => Some(Observation::new(
            Activity::Idle,
            BlockedOn::None,
            InputBuffer::Unknown,
        )),
        // `StopFailure` fires INSTEAD of `Stop` when an API error ended the turn (Claude Code's
        // own words, 2.1.259), at the same lifecycle point — so the categorical truth is the one
        // `Stop` writes and only the reason differs. Deliberately not `ended`: the TUI is still
        // live, a human can re-login and carry on, and this seat's terminal record belongs to the
        // wrapper (OHS-T04). A hook claiming `ended` here would be a false terminal.
        "StopFailure" => Some(
            Observation::new(Activity::Idle, BlockedOn::None, InputBuffer::Unknown).with_reason(
                match stop_failure_error(payload) {
                    Some(CLAUDE_AUTH_REJECTED_ERROR) => "providerAuth",
                    _ => "apiError",
                },
            ),
        ),
        "PermissionRequest" => {
            // Driver-side classification (#162): the payload's tool_name distinguishes Claude's
            // question form from an ordinary permission prompt — the DQ-H1 captures show
            // AskUserQuestion arriving as a PermissionRequest like any other tool.
            let ask = if payload.get("tool_name").and_then(serde_json::Value::as_str)
                == Some("AskUserQuestion")
            {
                Ask::Question
            } else {
                Ask::Permission
            };
            Some(
                Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
                    .with_ask(ask)
                    .with_reason("permissionRequest"),
            )
        }
        _ => None,
    }
}

/// The `StopFailure` error word that names a rejected provider credential.
///
/// Claude Code classifies every 401/403 provider response as `authentication_failed` (measured on
/// 2.1.259, which also documents the event as "fires instead of Stop when an API error (rate
/// limit, auth failure, etc.) ended the turn"), so this one word IS the credential-rejected class.
/// The siblings in that closed vocabulary are deliberately not it: `rate_limit` and `overloaded`
/// are capacity, `oauth_org_not_allowed` is an org policy no re-login can satisfy,
/// `account_on_hold` and `billing_error` are account state, and `invalid_request`,
/// `model_not_found`, `server_error`, `max_output_tokens` and `unknown` are request or server
/// faults. Naming any of them a credential rejection would hand an operator the wrong repair.
const CLAUDE_AUTH_REJECTED_ERROR: &str = "authentication_failed";

/// The closed `StopFailure` error word, as the payload spells it. `error_details` and
/// `last_assistant_message` ride the same payload and are deliberately untouched: they are prose.
fn stop_failure_error(payload: &serde_json::Value) -> Option<&str> {
    payload.get("error").and_then(serde_json::Value::as_str)
}

/// What one hook event proves about the seat's provider credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAuthEdge {
    Rejected,
    Accepted,
}

/// Read the credential edge out of one hook event, or `None` when the event proves nothing about
/// it — which must leave a standing rejection alone rather than clearing it.
fn provider_auth_edge(event: &str, payload: &serde_json::Value) -> Option<ProviderAuthEdge> {
    match event {
        "StopFailure" => (stop_failure_error(payload) == Some(CLAUDE_AUTH_REJECTED_ERROR))
            .then_some(ProviderAuthEdge::Rejected),
        // A turn that reached its ordinary end is positive proof the credential was accepted.
        // `SessionStart` is not: a fresh session has made no provider call yet.
        "Stop" => Some(ProviderAuthEdge::Accepted),
        _ => None,
    }
}

/// Record one credential edge on the seat's native-driver diagnostic.
///
/// Each hook invocation is its own short-lived writer, so the publisher's stage set starts empty
/// and its on-disk fallback is what lets a later `Stop` clear a rejection an earlier `StopFailure`
/// wrote from a different process. Fail-open like every other observation here: the publisher only
/// warns on a write it cannot land.
fn publish_provider_auth(agent_dir: &Path, edge: ProviderAuthEdge) {
    let mut publisher = driver_diagnostic::Publisher::new(
        agent_dir,
        driver_diagnostic::Driver::Claude,
        // A hook payload carries no Claude version — the common hook input is session id,
        // transcript path, cwd, prompt id, permission mode, agent identity and effort, and
        // nothing else (2.1.259) — and st2 gates no Claude version, so neither the producer
        // version nor its support status is knowable from here.
        None,
        driver_diagnostic::Support::Unknown,
    );
    match edge {
        ProviderAuthEdge::Rejected => publisher.publish(
            driver_diagnostic::Stage::ProviderAuth,
            driver_diagnostic::Reason::ProviderAuthRejected,
            driver_diagnostic::Source::TurnResult,
        ),
        ProviderAuthEdge::Accepted => publisher.clear(driver_diagnostic::Stage::ProviderAuth),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use super::*;
    use crate::harness_state::harness_state_path;

    #[test]
    fn only_the_packaged_channel_requests_the_installation_preflight() {
        assert!(requires_st2_channel(&[
            "claude".into(),
            "--channels".into(),
            "plugin:st2-channel@st2".into(),
        ]));
        assert!(!requires_st2_channel(&[
            "claude".into(),
            "--channels".into(),
            "plugin:other@marketplace".into(),
        ]));
    }

    #[test]
    fn development_channel_fallback_is_inline_and_keeps_the_provider_arguments() {
        let argv = vec![
            "claude".into(),
            "--model".into(),
            "sonnet".into(),
            "--channels".into(),
            "plugin:st2-channel@st2".into(),
            "prompt".into(),
        ];
        let output = development_channel_argv(
            argv,
            Path::new("/opt/st2/bin/st2"),
            Path::new("/var/lib/st2/catalog"),
            "host.worker",
        )
        .unwrap();
        assert_eq!(&output[..3], &["claude", "--model", "sonnet"]);
        assert_eq!(
            output.last().map(String::as_str),
            Some("prompt"),
            "the user prompt remains last"
        );
        let config_index = output.iter().position(|arg| arg == "--mcp-config").unwrap();
        let mcp: serde_json::Value = serde_json::from_str(&output[config_index + 1]).unwrap();
        assert_eq!(mcp["mcpServers"]["st2"]["command"], "/opt/st2/bin/st2");
        assert_eq!(
            mcp["mcpServers"]["st2"]["args"],
            serde_json::json!([
                "--catalog",
                "/var/lib/st2/catalog",
                "driver",
                "claude-mcp",
                "--identity",
                "host.worker"
            ])
        );
        assert!(
            output
                .iter()
                .any(|arg| arg == "--dangerously-load-development-channels=server:st2")
        );
        assert!(!output.iter().any(|arg| arg == "--channels"));
    }

    #[test]
    fn idle_provider_refreshes_presence_without_mcp_input() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = status::status_path(tmp.path());
        status::set_state(&presence, status::State::Available).unwrap();
        let before = fs::read_to_string(&presence).unwrap();
        let stop = AtomicBool::new(false);

        run_provider(
            "Claude",
            &presence,
            &["sh".into(), "-c".into(), "sleep 0.12".into()],
            &[],
            Duration::from_millis(25),
            Duration::from_millis(5),
            &stop,
            None,
        )
        .unwrap();

        let after = fs::read_to_string(&presence).unwrap();
        assert_ne!(after, before);
        assert_eq!(status::read_state(&presence), status::State::Available);
    }

    #[test]
    fn hook_events_map_to_observations_with_the_blocked_edges() {
        let none = serde_json::Value::Null;

        let blocked = observe_hook_event("PermissionRequest", &none).unwrap();
        assert_eq!(blocked.state, Activity::Active);
        assert_eq!(blocked.blocked_on, BlockedOn::Human);
        assert_eq!(blocked.reason.as_deref(), Some("permissionRequest"));

        // The exit edges: tool progress or a turn boundary clears the human hold.
        for event in ["PreToolUse", "PostToolUse"] {
            let cleared = observe_hook_event(event, &none).unwrap();
            assert_eq!(cleared.state, Activity::Active);
            assert_eq!(cleared.blocked_on, BlockedOn::None);
        }
        let stop = observe_hook_event("Stop", &none).unwrap();
        assert_eq!(stop.state, Activity::Idle);
        assert_eq!(stop.blocked_on, BlockedOn::None);

        assert_eq!(
            observe_hook_event("UserPromptSubmit", &none).unwrap().state,
            Activity::Active
        );
        assert_eq!(
            observe_hook_event("SessionStart", &none).unwrap().state,
            Activity::Idle
        );

        // Unmapped events say nothing rather than guessing.
        assert_eq!(observe_hook_event("Notification", &none), None);
        assert_eq!(observe_hook_event("SubagentStop", &none), None);
    }

    /// The measured `StopFailure` payload shape (Claude Code 2.1.259: `hook_event_name`, the
    /// closed `error` word, optional `error_details` and `last_assistant_message`). It fires
    /// INSTEAD of `Stop`, so the turn is over whatever the word is — but only the
    /// credential class earns the `providerAuth` reason, and neither quota nor an org policy may
    /// borrow it: a re-login fixes exactly one of the three.
    #[test]
    fn stop_failure_classifies_only_the_credential_class_as_provider_auth() {
        let rejected = serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "authentication_failed",
            "error_details": "Please run /login",
            "last_assistant_message": "",
        });
        let rate_limited = serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "rate_limit",
        });
        let org_policy = serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "oauth_org_not_allowed",
        });

        for (payload, reason) in [
            (&rejected, "providerAuth"),
            (&rate_limited, "apiError"),
            (&org_policy, "apiError"),
        ] {
            let observed = observe_hook_event("StopFailure", payload).unwrap();
            assert_eq!(observed.state, Activity::Idle, "the turn ended: {reason}");
            assert_eq!(observed.blocked_on, BlockedOn::None);
            assert_eq!(observed.reason.as_deref(), Some(reason));
        }

        assert_eq!(
            provider_auth_edge("StopFailure", &rejected),
            Some(ProviderAuthEdge::Rejected)
        );
        assert_eq!(
            provider_auth_edge("StopFailure", &rate_limited),
            None,
            "an exhausted allowance is not a rejected credential"
        );
        assert_eq!(
            provider_auth_edge("StopFailure", &org_policy),
            None,
            "an org policy no re-login can satisfy is not a rejected credential"
        );
        assert_eq!(
            provider_auth_edge("Stop", &serde_json::Value::Null),
            Some(ProviderAuthEdge::Accepted),
            "a turn that reached its ordinary end proves the credential worked"
        );
        assert_eq!(
            provider_auth_edge("SessionStart", &serde_json::Value::Null),
            None,
            "a fresh session has made no provider call to prove anything with"
        );
    }

    /// Each hook invocation is its own process, so the record must survive between them and the
    /// recovery edge must reach a failure a different process published.
    #[test]
    fn a_rejected_claude_credential_stands_until_a_turn_reaches_its_ordinary_end() {
        let tmp = tempfile::tempdir().unwrap();
        let record = driver_diagnostic::path(tmp.path());
        let rejected = serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "authentication_failed",
        });

        publish_provider_auth(
            tmp.path(),
            provider_auth_edge("StopFailure", &rejected).unwrap(),
        );
        let driver_diagnostic::Observed::Failure(failure) = driver_diagnostic::read(&record) else {
            panic!("a rejected credential must publish a native-driver diagnostic")
        };
        assert_eq!(failure.driver, driver_diagnostic::Driver::Claude);
        assert_eq!(failure.stage, driver_diagnostic::Stage::ProviderAuth);
        assert_eq!(
            failure.reason,
            driver_diagnostic::Reason::ProviderAuthRejected
        );
        assert_eq!(failure.source, driver_diagnostic::Source::TurnResult);
        assert_eq!(
            failure.producer_version, None,
            "a hook payload names no Claude version"
        );
        assert_eq!(
            failure.support,
            driver_diagnostic::Support::Unknown,
            "st2 gates no Claude version, so support is not knowable from a hook"
        );

        // A later quota failure carries no credential edge, so the rejection stands.
        let rate_limited = serde_json::json!({
            "hook_event_name": "StopFailure",
            "error": "rate_limit",
        });
        assert_eq!(provider_auth_edge("StopFailure", &rate_limited), None);
        assert!(matches!(
            driver_diagnostic::read(&record),
            driver_diagnostic::Observed::Failure(_)
        ));

        publish_provider_auth(tmp.path(), ProviderAuthEdge::Accepted);
        assert_eq!(
            driver_diagnostic::read(&record),
            driver_diagnostic::Observed::Absent,
            "the next ordinary turn end clears a rejection a sibling hook process published"
        );
    }

    #[test]
    fn subagent_events_never_move_top_level_state() {
        let subagent = serde_json::json!({"agent_id": "sub-1", "agent_type": ""});
        for event in [
            "Stop",
            "UserPromptSubmit",
            "PermissionRequest",
            "PostToolUse",
        ] {
            assert_eq!(observe_hook_event(event, &subagent), None, "{event}");
        }
        // An empty agent_id is the top-level shape.
        let top = serde_json::json!({"agent_id": ""});
        assert!(observe_hook_event("Stop", &top).is_some());
    }

    /// The measured grant-path sequence from the DQ-H1 capture (2026-08-23, Claude Code 2.1.237,
    /// `docs/vrs/05-harness-state/.experiments/2026-08-23-claude-batched-permission.md`): in a
    /// two-call batch where the first call needs permission, execution serializes around the open
    /// prompt, so the event after `PermissionRequest` is the granted call's own `PostToolUse` and
    /// the exit rule clears `blocked` at exactly the right moment. Replayed verbatim so a future
    /// mapping change that breaks the measured sequence fails here, not in the field.
    #[test]
    fn measured_batched_grant_sequence_holds_blocked_until_the_granted_calls_own_post() {
        let pre_touch = serde_json::json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash",
            "tool_input": {"command": "touch scratch2.txt"},
            "tool_use_id": "toolu_01HK5aLKavjdbrCk48cfd58k",
            "prompt_id": "0ea832de-ece1-4575-900f-4dab5e2f6849",
        });
        let permission_request = serde_json::json!({
            "hook_event_name": "PermissionRequest", "tool_name": "Bash",
            "tool_input": {"command": "touch scratch2.txt"},
            "permission_suggestions": [],
            "prompt_id": "0ea832de-ece1-4575-900f-4dab5e2f6849",
        });
        let post_touch = serde_json::json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": {"command": "touch scratch2.txt"},
            "tool_use_id": "toolu_01HK5aLkavjdbrCk48cfd58k",
            "prompt_id": "0ea832de-ece1-4575-900f-4dab5e2f6849",
        });
        // The phantom SubagentStop trailing the turn: non-empty agent_id, EMPTY agent_type, no
        // subagent ran — the exact emergent shape the guard keys on, reproduced in this build.
        let phantom_subagent_stop = serde_json::json!({
            "hook_event_name": "SubagentStop",
            "agent_id": "a5c61ec4ef268c3cc", "agent_type": "",
            "prompt_id": "0ea832de-ece1-4575-900f-4dab5e2f6849",
        });

        let entered = observe_hook_event("PermissionRequest", &permission_request).unwrap();
        assert_eq!(
            (entered.state, entered.blocked_on),
            (Activity::Active, BlockedOn::Human)
        );
        // 33 s of open prompt produced no intervening event in the capture; the very next event
        // is the granted call's own PostToolUse, which correctly releases the block.
        let released = observe_hook_event("PostToolUse", &post_touch).unwrap();
        assert_eq!(
            (released.state, released.blocked_on),
            (Activity::Active, BlockedOn::None)
        );
        let _ = observe_hook_event("PreToolUse", &pre_touch);
        assert_eq!(
            observe_hook_event("SubagentStop", &phantom_subagent_stop),
            None,
            "the phantom SubagentStop must not resurrect activity after Stop"
        );
    }

    #[test]
    fn wrapper_heartbeat_re_stamps_without_clobbering_hook_written_state() {
        let tmp = tempfile::tempdir().unwrap();
        let record = harness_state_path(tmp.path());
        let observer =
            SessionObserver::new(tmp.path(), "hetz.worker", "claude", "hetz.worker").unwrap();

        // A hook process wrote a blocked observation between wrapper ticks — carrying the
        // wrapper's exported token, exactly as the env plumbing arranges in a real seat.
        harness_state::Writer::new(
            tmp.path(),
            "hetz.worker",
            "claude",
            Some("hetz.worker".to_string()),
        )
        .with_session(observer.session())
        .observe(observe_hook_event("PermissionRequest", &serde_json::Value::Null).unwrap())
        .unwrap();
        let before = fs::read(&record).unwrap();

        std::thread::sleep(Duration::from_millis(2));
        observer.heartbeat();
        let after = fs::read(&record).unwrap();
        assert_ne!(before, after, "heartbeat must re-stamp bytes");
        let observed = harness_state::read(&record, None).unwrap();
        assert_eq!(observed.state, Activity::Active);
        assert_eq!(observed.blocked_on, BlockedOn::Human);
        assert_eq!(observed.reason.as_deref(), Some("permissionRequest"));
    }

    #[test]
    fn a_provider_killed_mid_turn_reads_ended_rather_than_active() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = status::status_path(tmp.path());
        let record = harness_state_path(tmp.path());
        let observer =
            SessionObserver::new(tmp.path(), "hetz.worker", "claude", "hetz.worker").unwrap();
        let stop = AtomicBool::new(false);

        // A turn is in flight when the provider dies by signal.
        harness_state::Writer::new(
            tmp.path(),
            "hetz.worker",
            "claude",
            Some("hetz.worker".to_string()),
        )
        .observe(observe_hook_event("UserPromptSubmit", &serde_json::Value::Null).unwrap())
        .unwrap();

        let result = run_provider(
            "Claude",
            &presence,
            &["sh".into(), "-c".into(), "kill -9 $$".into()],
            &[],
            Duration::from_millis(25),
            Duration::from_millis(5),
            &stop,
            Some(&observer),
        );
        assert!(result.is_err(), "a signalled provider is a failed run");

        let observed = harness_state::read(&record, None).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }

    #[test]
    fn a_clean_provider_exit_writes_the_terminal_record() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = status::status_path(tmp.path());
        let observer =
            SessionObserver::new(tmp.path(), "hetz.worker", "claude", "hetz.worker").unwrap();
        let stop = AtomicBool::new(false);

        run_provider(
            "Claude",
            &presence,
            &["true".into()],
            &[],
            Duration::from_millis(25),
            Duration::from_millis(5),
            &stop,
            Some(&observer),
        )
        .unwrap();

        let observed = harness_state::read(&harness_state_path(tmp.path()), None).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("exit 0"));
    }

    #[test]
    fn permission_requests_classify_their_ask_kind_from_the_tool_name() {
        use crate::harness_state::Ask;
        let permission = observe_hook_event(
            "PermissionRequest",
            &serde_json::json!({ "tool_name": "Bash", "tool_input": {} }),
        )
        .unwrap();
        assert_eq!(permission.ask, Ask::Permission);

        let question = observe_hook_event(
            "PermissionRequest",
            &serde_json::json!({ "tool_name": "AskUserQuestion", "tool_input": {} }),
        )
        .unwrap();
        assert_eq!(question.ask, Ask::Question);

        // Non-blocking events carry no ask.
        let idle = observe_hook_event("Stop", &serde_json::json!({})).unwrap();
        assert_eq!(idle.ask, Ask::None);
    }

    /// T2: a hook that finishes after the wrapper reaped Claude must not replace the terminal
    /// record — the wrapper's `ended` carries the shared token and is the session's last word —
    /// while a NEW session's boundary event still supersedes an old terminal record.
    #[test]
    fn a_late_hook_never_overwrites_this_sessions_terminal_record() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let record = harness_state_path(tmp.path());
        let observer =
            SessionObserver::new(tmp.path(), "hetz.worker", "claude", "hetz.worker").unwrap();
        observer.ended("exit 0");

        // The straggler hook shares the session token (env plumbing) and is suppressed.
        let mut late = harness_state::Writer::new(
            tmp.path(),
            "hetz.worker",
            "claude",
            Some("hetz.worker".to_string()),
        )
        .with_session(observer.session());
        assert!(
            !late
                .observe_unless_ended(
                    observe_hook_event("PostToolUse", &serde_json::Value::Null).unwrap()
                )
                .unwrap()
        );
        assert_eq!(
            harness_state::read(&record, None).unwrap().state,
            Activity::Ended
        );

        // A wrapperless fresh session (token-only, the session_id fallback) cannot take over a
        // claimed record: only a written claim supersedes.
        let mut fallback = harness_state::Writer::new(
            tmp.path(),
            "hetz.worker",
            "claude",
            Some("hetz.worker".to_string()),
        )
        .with_session("claude-session-fresh");
        fallback.interrupt();
        assert!(
            !fallback
                .observe_unless_ended(
                    observe_hook_event("SessionStart", &serde_json::Value::Null).unwrap()
                )
                .unwrap()
        );
        assert_eq!(
            harness_state::read(&record, None).unwrap().state,
            Activity::Ended
        );

        // A claimed new session — the wrapper path — supersedes the old terminal record.
        let next =
            SessionObserver::new(tmp.path(), "hetz.worker", "claude", "hetz.worker").unwrap();
        let mut fresh = harness_state::Writer::new(
            tmp.path(),
            "hetz.worker",
            "claude",
            Some("hetz.worker".to_string()),
        )
        .with_ownership(next.session().to_string(), next.seq());
        assert!(
            fresh
                .observe_unless_ended(
                    observe_hook_event("SessionStart", &serde_json::Value::Null).unwrap()
                )
                .unwrap()
        );
        assert_eq!(
            harness_state::read(&record, None).unwrap().state,
            Activity::Idle
        );
    }

    /// W8-12: two sessions of a WRAPPERLESS seat. Session A's hooks write; session B's
    /// SessionStart performs the written claim and takes over; A's straggler is refused and B's
    /// later hooks adopt B's records.
    #[test]
    fn a_wrapperless_seat_survives_its_own_session_succession() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let record = harness_state_path(tmp.path());
        let payload_a = serde_json::json!({ "session_id": "aaa" });
        let payload_b = serde_json::json!({ "session_id": "bbb" });
        let drive = |event: &str, payload: &serde_json::Value| {
            let mut writer =
                observe_writer(tmp.path(), "hetz.worker", None, event, payload, None, None);
            writer
                .observe_unless_ended(observe_hook_event(event, payload).unwrap())
                .unwrap()
        };

        assert!(drive("SessionStart", &payload_a), "A claims");
        assert!(drive("UserPromptSubmit", &payload_a), "A's hooks adopt");
        assert_eq!(
            harness_state::read(&record, None).unwrap().state,
            Activity::Active
        );

        assert!(drive("SessionStart", &payload_b), "B claims over A");
        assert!(
            !drive("PostToolUse", &payload_a),
            "A's straggler is refused"
        );
        assert!(drive("UserPromptSubmit", &payload_b), "B's hooks adopt");
        assert_eq!(
            harness_state::read(&record, None).unwrap().state,
            Activity::Active
        );
    }

    /// The status-line payload captured verbatim from the version in the producer table, before
    /// the session's first API response.
    const PRE_TURN: &str =
        include_str!("../tests/fixtures/harness-context/claude-statusline-pre-turn.json");
    /// The same captured 2.1.250 envelope with a real `current_usage` object composed into it.
    ///
    /// **Composition, stated because it matters for what this fixture proves.** The envelope, its
    /// `context_window_size`, and its `rate_limits` are the verbatim 2.1.250 live capture; the
    /// `current_usage` object is a verbatim `usage` object off an assistant line of a real
    /// 2.1.250 transcript (`claude-opus-5`, 2026-08-29T11:46:14Z), and `used_percentage` /
    /// `total_input_tokens` are what the bundle's own builder computes from the two:
    ///
    /// ```js
    /// total_input_tokens: d.input_tokens + d.cache_creation_input_tokens + d.cache_read_input_tokens
    /// used: Math.min(100, Math.max(0, Math.round(r / t * 100)))
    /// ```
    ///
    /// So the numerator and the denominator are each measured, and the arithmetic joining them is
    /// quoted from the harness rather than inferred. What this fixture does NOT prove is that
    /// 2.1.250 emits exactly these bytes together in one payload — a populated live capture needs
    /// a paid turn and none was taken. A bump that moves the numerator's terms or the percent rule
    /// still fails here, which is what HC-R13 asks of it.
    const MID_SESSION: &str =
        include_str!("../tests/fixtures/harness-context/claude-statusline-mid-session.json");

    fn fixture(raw: &str) -> serde_json::Value {
        let payload: serde_json::Value = serde_json::from_str(raw).unwrap();
        // HC-R13: the version is asserted literally, so a fixture recaptured from a different
        // build cannot quietly keep proving the old arithmetic.
        assert_eq!(
            payload.get("version").and_then(serde_json::Value::as_str),
            Some(STATUSLINE_VERSION)
        );
        payload
    }

    #[test]
    fn a_mid_session_statusline_payload_yields_claudes_own_triple() {
        let reading = statusline_reading(&fixture(MID_SESSION));

        // input 2 + cache_creation 2837 + cache_read 191924, read off `current_usage` rather
        // than off the sibling `total_input_tokens` that agrees with it here.
        assert_eq!(reading.used_tokens, Some(194_763));
        assert_eq!(reading.window_tokens, Some(1_000_000));
        // Claude's own integer, taken as published: st2 never divides the operands to make one.
        assert_eq!(reading.used_percent, Some(19.0));
        assert_eq!(reading.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(reading.cost_usd, Some(4.7312));
        assert_eq!(reading.rate_limits.five_hour, Some(31.0));
        assert_eq!(reading.rate_limits.seven_day, Some(55.0));
        // The payload's `total_*` keys describe the LAST RESPONSE, so nothing here is cumulative
        // session spend and the field stays null rather than being fed a number that would read
        // as occupancy on division (HC-R16).
        assert_eq!(reading.session_total_tokens, None);
    }

    #[test]
    fn a_pre_turn_statusline_payload_withholds_rather_than_reporting_zero() {
        let reading = statusline_reading(&fixture(PRE_TURN));

        // `current_usage` is null: Claude is positively declaring it does not yet know its own
        // occupancy, and the producer withholds (HC-R03). The trap is the sibling
        // `total_input_tokens`, which the bundle's builder emits as 0 for exactly this state —
        // a zero DERIVED FROM THE ABSENCE, which a producer reading it would publish as a
        // measurement of an empty window.
        assert_eq!(reading.used_tokens, None);
        assert_eq!(reading.used_percent, None);
        // The window is populated from the start and is reported (HC-R02): a legal record with a
        // window and no percent.
        assert_eq!(reading.window_tokens, Some(1_000_000));
        // Zero cost is an observation, not an absence: a producer filtering it to null would
        // lose the distinction between "free so far" and "not reported".
        assert_eq!(reading.cost_usd, Some(0.0));
        assert_eq!(reading.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(reading.rate_limits.five_hour, Some(31.0));
    }

    fn agent_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        // One level down, so the writer has a parent to stage in outside the agent subtree.
        let dir = tmp.path().join("agents/Silber/fabric");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn context(dir: &Path) -> crate::harness_context::Observed {
        harness_context::read(&harness_context::harness_context_path(dir)).unwrap()
    }

    #[test]
    fn a_pre_turn_reading_lands_once_and_then_sits_inside_its_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_dir(&tmp);
        let payload = fixture(PRE_TURN);

        let mut writer = context_writer(&dir, "Silber.fabric", &payload).unwrap();
        assert!(writer.observe(statusline_reading(&payload)).unwrap());
        // A withheld percent has no bucket, so a second identical render is inside the written
        // one and, well inside the heartbeat, writes nothing. That is the write guard (HC-R09)
        // proved on the Claude path: a 5-second refresh interval does not mean 720 writes an hour.
        assert!(!writer.observe(statusline_reading(&payload)).unwrap());

        let observed = context(&dir);
        assert_eq!(observed.harness, Harness::Claude);
        assert_eq!(observed.used_percent, None);
        assert_eq!(observed.window_tokens, Some(1_000_000));
        assert_eq!(observed.compactions, 0);
    }

    #[test]
    fn claudes_three_compaction_edges_count_one_compaction() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_dir(&tmp);
        let session = serde_json::json!({"session_id": "s-1"});
        let mut payload = session.clone();
        payload["trigger"] = "auto".into();

        observe_compaction(&dir, "Silber.fabric", "PreCompact", &payload).unwrap();
        let counted = context(&dir);
        assert_eq!(counted.compactions, 1);
        assert_eq!(
            counted.last_compaction_trigger,
            Some(CompactionTrigger::Auto)
        );

        // The completion edge holds the count and only moves `lastCompactionMs` forward.
        observe_compaction(&dir, "Silber.fabric", "PostCompact", &payload).unwrap();
        let completed = context(&dir);
        assert_eq!(
            completed.compactions, 1,
            "PostCompact must not double-count"
        );
        assert!(completed.last_compaction_ms >= counted.last_compaction_ms);

        // The third sighting of the same compaction. `SessionStart source=compact` is recognized
        // and deliberately inert — counting it is exactly the double count HC-R12 forbids.
        let mut restart = session;
        restart["source"] = "compact".into();
        observe_compaction(&dir, "Silber.fabric", "SessionStart", &restart).unwrap();
        assert_eq!(context(&dir).compactions, 1);
    }

    #[test]
    fn a_post_compact_without_a_counted_predecessor_counts_the_compaction_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_dir(&tmp);
        let payload = serde_json::json!({"session_id": "s-1", "trigger": "manual"});

        // No record at all: the PreCompact write never landed, so PostCompact is the first
        // evidence st2 has that a compaction happened and it counts rather than losing it.
        observe_compaction(&dir, "Silber.fabric", "PostCompact", &payload).unwrap();
        let observed = context(&dir);
        assert_eq!(observed.compactions, 1);
        assert_eq!(
            observed.last_compaction_trigger,
            Some(CompactionTrigger::Manual)
        );
    }

    #[test]
    fn a_subagents_compaction_never_touches_the_top_level_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_dir(&tmp);
        let payload = serde_json::json!({
            "session_id": "s-1", "trigger": "auto", "agent_id": "sub-7"
        });

        observe_compaction(&dir, "Silber.fabric", "PreCompact", &payload).unwrap();
        assert!(harness_context::read(&harness_context::harness_context_path(&dir)).is_none());
    }

    #[test]
    fn an_unrecognized_trigger_word_decodes_as_unknown_not_as_a_definite_one() {
        assert_eq!(
            compaction_trigger(&serde_json::json!({"trigger": "auto"})),
            CompactionTrigger::Auto
        );
        assert_eq!(
            compaction_trigger(&serde_json::json!({"trigger": "manual"})),
            CompactionTrigger::Manual
        );
        // Additive tolerance in the direction that matters: a future word must not be guessed
        // into the closed vocabulary as if the harness had said it.
        assert_eq!(
            compaction_trigger(&serde_json::json!({"trigger": "idle"})),
            CompactionTrigger::Unknown
        );
        assert_eq!(
            compaction_trigger(&serde_json::json!({})),
            CompactionTrigger::Unknown
        );
    }

    #[test]
    fn the_tees_incarnation_is_the_same_token_the_hooks_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = agent_dir(&tmp);
        let payload = serde_json::json!({"session_id": "abc"});

        // HC-A03: the wrapper's hook subprocesses and its status-line tee are driver processes of
        // ONE session, and they must publish one incarnation or a reader could not tell a
        // straggler from a sibling. Both derive it from the same payload field by the same rule.
        assert_eq!(
            wrapperless_token(&payload).as_deref(),
            Some("claude-session-abc")
        );
        context_writer(&dir, "Silber.fabric", &payload)
            .unwrap()
            .observe(statusline_reading(&payload))
            .unwrap();
        let raw = fs::read_to_string(harness_context::harness_context_path(&dir)).unwrap();
        assert!(
            raw.contains("\"incarnation\":\"claude-session-abc\""),
            "{raw}"
        );
    }
}
