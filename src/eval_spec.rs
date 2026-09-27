//! The **st2 spec** — the native, legible eval/agent format (`st2 eval ./folder`), nailed down in
//! review. One `.kdl` file describes a compact team, or explicitly selects canonical Agent Specs
//! already materialized in the eval's hermetic catalog, and an optional `eval` that copies a
//! fixture, kicks off the team, waits for the sup's confirmation, and runs judges. This module is
//! the PARSER + model (P1); `st2 up`/`st2 eval` (team boot + eval flow + judge engine) build on it.
//!
//! Principles (from the design — do not violate): legible top-to-bottom, everything in one file, st2
//! self-contained (generated DING binds the running st2 executable instead of resolving st2 on PATH).
//! Ergonomic collapses (review-driven): a bare `ding` node is the built-in sidecar (auto-prefixed
//! `team.agent.ding`, `--root $ST_ROOT`) — dedicated, so `exec` reserves no magic name;
//! `exec`/custom labels auto-prefix to `team.agent.<leaf>`. (`ding` → `ping` later.)

use std::collections::BTreeMap;
use std::time::Duration;

use kdl::{KdlDocument, KdlNode, KdlValue};

use agent_spec::spec::{
    AGENT_DESCRIPTION_MAX_CHARS, AGENT_NAME_MAX_CHARS, ClaudeDriver, CodexDriver, Driver, Restart,
    RestartMode, parse_duration, validate_presentation,
};

/// A parsed st2 spec: a base team (`st2 up` boots this) plus an optional `eval` (`st2 eval` runs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// The logical host this file's team belongs to (top-level `host "silber"`), if declared. It is the
    /// Logical host name, not necessarily the OS hostname. `st2 up/down` resolve the run host as
    /// `--host` (explicit) › this field › the OS hostname, so a per-host spec runs its own slice
    /// even when the machine's OS hostname differs from the logical name.
    pub host: Option<String>,
    /// Top-level env — cascades into every agent + process (child scopes override).
    pub env: BTreeMap<String, String>,
    /// The base team's agents, with team-prefixed ids and their env already cascaded.
    pub agents: Vec<SpecAgent>,
    /// The `eval { }` block, if present.
    pub eval: Option<Eval>,
}

/// One agent — the label IS the id (no separate identity/host/type). An agent has one launch source:
/// a legacy `command`, or one native `harness "claude" {}` / `harness "codex" {}` driver. Extra
/// processes follow as `exec` blocks. The provider-named driver blocks remain accepted as aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecAgent {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub workspace: Option<String>,
    /// This agent's supervisor (the id its crash escalates to). The chain of `supervisor` fields is
    /// walked to the root (the root's is `None` — that is the cos) for crash-ding escalation.
    pub supervisor: Option<String>,
    /// Fully-cascaded env for the main pty (top → team(s) → agent).
    pub env: BTreeMap<String, String>,
    pub command: Option<String>,
    pub driver: Option<Driver>,
    pub execs: Vec<SpecExec>,
}

/// An extra process under an agent (inherits the agent's cascaded env, plus its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecExec {
    pub id: String,
    pub command: String,
    pub env: BTreeMap<String, String>,
    /// The process came from built-in shorthand rather than an authored `exec` command.
    pub derived: bool,
}

/// The `eval { }` block — only `st2 eval` runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eval {
    /// `copy "./fixture"` — copy its CONTENTS 1:1 into the temp catalog (the start world). `_git` dirs
    /// are renamed to `.git` on copy so a fixture can ship a real repo (see the eval flow, P3).
    pub copy: Option<String>,
    /// The kickoff message — identical to `st2 message send`. `None` for a TEAM-LESS eval (jobs + judges,
    /// no team to kick off).
    pub message: Option<Kick>,
    /// The whole-eval timeout cap.
    pub max_timeout: Duration,
    /// Eval-only agents (e.g. a judge) — an eval may boot MORE agents than the base team.
    pub agents: Vec<SpecAgent>,
    /// The `run` stage (act): command steps run to completion (sequentially, in declaration order)
    /// BEFORE judging. arrange (copy) → act (run) → assert (judges). By DEFAULT a step must exit 0
    /// (setup should succeed) and a non-zero final exit hard-fails the verdict; `allow-nonzero` opts a
    /// step OUT (when a non-zero exit is the expected pass, e.g. a health-check on a malformed input),
    /// leaving the exit for a judge to assert. Written flat, one node per step: `run "label" { command … }`.
    pub run_steps: Vec<RunStep>,
    /// All must pass.
    pub judges: Vec<Judge>,
    /// Opt-in: SUPERVISE the team during the run (a bare `supervise` directive in the eval block). When
    /// set, a dead team seat is respawned FROM THE SPEC (full env re-applied → it rejoins the bus cold),
    /// so a fault-injector can restart/crash a seat mid-run and the recovery is exercised. Absent →
    /// boot-once (the default; the seats are booted once and not respawned) — unchanged for every other
    /// eval. Recovery remains respawn-from-spec rather than `pty restart`, so eval supervision stays
    /// owned by st2 even though PTY metadata can now restore the managed environment manually.
    pub supervise: bool,
    /// Opt-in: after `copy` and `run` finish, discover the eval team from canonical Agent Specs in
    /// the hermetic catalog. Mutually exclusive with the compact `team` / `agent` grammar: one eval
    /// has one declaration authority.
    pub canonical_agents: bool,
}

/// The kickoff. `content` is a file path (relative to the spec folder) or inline text — resolved at
/// eval time (file-if-it-exists, else inline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kick {
    pub from: String,
    pub to: String,
    pub content: String,
}

/// One command step of the `run` stage: run `command` to completion (cwd = `workspace` in the copied
/// catalog), capture stdout/stderr/exit for the judges, retry on non-zero per `retry`. By default a
/// non-zero final exit hard-fails the eval; `allow_nonzero` opts out and leaves the exit to the judges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStep {
    /// The step id (its capture files are `$CATALOG/.runs/<id>.{out,err,exit}`; its exit is exported as
    /// `RUN_<id>_EXIT` to later steps + the judges).
    pub id: String,
    /// cwd, resolved relative to the catalog (the copied world). `None` → the catalog root.
    pub workspace: Option<String>,
    /// The command, run via `sh -c` (so `$CATALOG`/`$JOBS_DIR` expand).
    pub command: String,
    /// Per-job env OVER the top-level cascade (child wins).
    pub env: BTreeMap<String, String>,
    /// Vars to UNSET from the cascade for this job (e.g. run a command with `ST_ROOT` unset).
    pub unset: Vec<String>,
    /// Retry-on-nonzero policy (attempts + inter-retry backoff). `None` → run once, no retry.
    pub retry: Option<Restart>,
    /// Opt-OUT: by default a non-zero FINAL exit (after retries) hard-fails the eval (setup should
    /// succeed). `allow-nonzero` accepts a non-zero exit as fine, leaving it for a judge to assert.
    pub allow_nonzero: bool,
}

/// A judge — all must pass; each is exactly one flavor, with an optional per-judge timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judge {
    pub name: String,
    pub timeout: Option<Duration>,
    pub kind: JudgeKind,
    /// A SIGNAL judge (a bare `signal` directive): it RUNS and its result is shown in the report, but it
    /// does NOT count toward the SCORE or the verdict — a show-but-don't-gate tier for secondary info that
    /// would otherwise inflate an all-must-pass score as a hollow always-pass.
    pub signal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeKind {
    /// Declarative file/json/committed checks (all must hold).
    Declarative(Vec<Check>),
    /// A shell command; exit 0 = pass. Runs via `sh -c`, so `bash …`/shebangs are honored.
    Bash(String),
    /// Ask a real judge agent and read PASS/FAIL from its reply.
    Ask { agent: String, prompt: String },
}

/// One declarative check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// `file "p" has "text"` — path (relative to the temp catalog root) contains the substring.
    FileHas { path: String, text: String },
    /// `file "p" lacks "text"` — path does NOT contain the substring.
    FileLacks { path: String, text: String },
    /// `json "p" field "x" is "y"` — the JSON field equals the value.
    JsonField {
        path: String,
        field: String,
        value: JsonScalar,
    },
    /// `committed "p"` — the path is tracked + committed (checked by the eval flow via git).
    Committed { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonScalar {
    String(String),
    Bool(bool),
    Integer(i64),
}

// ── KDL helpers (match the idiom in render.rs / kdl_format.rs) ───────────────────────────────────

/// The node's first positional argument as a string.
fn arg(node: &KdlNode) -> Option<String> {
    node.get(0).and_then(|v| v.as_string()).map(String::from)
}

/// The nth positional (string-valued) argument.
fn arg_n(node: &KdlNode, n: usize) -> Option<String> {
    node.get(n).and_then(|v| v.as_string()).map(String::from)
}

/// The first positional argument as a `u32` (KDL bare integer, e.g. `attempts 3`).
fn arg_u32(node: &KdlNode) -> Option<u32> {
    node.get(0)
        .and_then(|v| v.as_integer())
        .and_then(|i| u32::try_from(i).ok())
}

/// A child node's first argument, by child name.
fn child_arg(node: &KdlNode, name: &str) -> Option<String> {
    node.children()?
        .nodes()
        .iter()
        .find(|n| n.name().value() == name)
        .and_then(arg)
}

fn parse_agent_presentation(
    node: &KdlNode,
    agent_id: &str,
    field: &str,
    destination: &mut Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        destination.is_none(),
        "agent '{agent_id}' declares `{field}` more than once"
    );
    anyhow::ensure!(
        node.children().is_none()
            && node.entries().len() == 1
            && node.entries()[0].name().is_none(),
        "agent '{agent_id}' `{field}` must contain exactly one positional string"
    );
    let value = arg(node)
        .ok_or_else(|| anyhow::anyhow!("agent '{agent_id}' `{field}` must contain a string"))?;
    *destination = Some(value);
    Ok(())
}

fn driver_string(node: &KdlNode, provider: &str, field: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        node.children().is_none()
            && node.entries().len() == 1
            && node.entries()[0].name().is_none(),
        "agent `{provider}.{field}` must contain exactly one positional string"
    );
    arg(node).ok_or_else(|| {
        anyhow::anyhow!("agent `{provider}.{field}` must contain exactly one positional string")
    })
}

fn driver_args(node: &KdlNode, provider: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        node.children().is_none() && node.entries().iter().all(|entry| entry.name().is_none()),
        "agent `{provider}.args` must contain only positional strings"
    );
    node.entries()
        .iter()
        .map(|entry| {
            entry.value().as_string().map(String::from).ok_or_else(|| {
                anyhow::anyhow!("agent `{provider}.args` must contain only positional strings")
            })
        })
        .collect()
}

fn driver_bool(node: &KdlNode, provider: &str, field: &str) -> anyhow::Result<bool> {
    anyhow::ensure!(
        node.children().is_none()
            && node.entries().len() == 1
            && node.entries()[0].name().is_none(),
        "agent `{provider}.{field}` must contain exactly one positional bool"
    );
    node.get(0)
        .and_then(|value| value.as_bool())
        .ok_or_else(|| {
            anyhow::anyhow!("agent `{provider}.{field}` must contain exactly one positional bool")
        })
}

type CommonDriverFields = (Option<String>, Option<String>, bool, String, Vec<String>);

fn parse_driver_fields(
    node: &KdlNode,
    provider: &str,
    allow_dev_channels: bool,
) -> anyhow::Result<CommonDriverFields> {
    anyhow::ensure!(
        node.entries().is_empty() && node.children().is_some(),
        "agent `{provider}` must be a child block without entries"
    );
    let mut model = None;
    let mut effort = None;
    let mut dev_channels = None;
    let mut prompt = None;
    let mut args = None;
    for child in node.children().expect("checked above").nodes() {
        match child.name().value() {
            "model" => {
                anyhow::ensure!(model.is_none(), "agent `{provider}` has duplicate `model`");
                model = Some(driver_string(child, provider, "model")?);
            }
            "effort" => {
                anyhow::ensure!(
                    effort.is_none(),
                    "agent `{provider}` has duplicate `effort`"
                );
                effort = Some(driver_string(child, provider, "effort")?);
            }
            "dev-channels" if allow_dev_channels => {
                anyhow::ensure!(
                    dev_channels.is_none(),
                    "agent `{provider}` has duplicate `dev-channels`"
                );
                dev_channels = Some(driver_bool(child, provider, "dev-channels")?);
            }
            "prompt" => {
                anyhow::ensure!(
                    prompt.is_none(),
                    "agent `{provider}` has duplicate `prompt`"
                );
                prompt = Some(driver_string(child, provider, "prompt")?);
            }
            "args" => {
                anyhow::ensure!(args.is_none(), "agent `{provider}` has duplicate `args`");
                args = Some(driver_args(child, provider)?);
            }
            other => anyhow::bail!("agent `{provider}` has unsupported field `{other}`"),
        }
    }
    Ok((
        model,
        effort,
        dev_channels.unwrap_or(false),
        prompt.ok_or_else(|| anyhow::anyhow!("agent `{provider}` requires `prompt`"))?,
        args.unwrap_or_default(),
    ))
}

fn parse_native_driver(node: &KdlNode) -> anyhow::Result<Driver> {
    let provider = node.name().value();
    let (model, effort, dev_channels, prompt, args) =
        parse_driver_fields(node, provider, provider == "claude")?;
    match provider {
        "claude" => Ok(Driver::Claude(ClaudeDriver {
            model,
            effort,
            dev_channels,
            prompt,
            args,
        })),
        "codex" => Ok(Driver::Codex(CodexDriver {
            model,
            effort,
            prompt,
            args,
        })),
        _ => unreachable!("the eval grammar only calls this helper for native providers"),
    }
}

/// Parse a flat `run "label" { … }` node into one [`RunStep`], appended in order. The `run` node IS
/// one step: its arg is the label, its body holds the command directly. Multiple `run "label"` nodes in
/// the eval block run in file order. (The old nested `run { step … }` wrapper was retired once every
/// eval moved to the flat form — a `run` with no label is now an error.)
fn parse_run_stage(node: &KdlNode, out: &mut Vec<RunStep>) -> anyhow::Result<()> {
    let label = arg(node).ok_or_else(|| {
        anyhow::anyhow!("run needs a label: write `run \"label\" {{ command … }}`")
    })?;
    out.push(parse_step_body(node, label)?);
    Ok(())
}

/// Parse a flat `run "id" { … }` node's BODY: `{ workspace; command; env{}; unset "V" …; retry{};
/// allow-nonzero }`. By default the step must exit 0; `allow-nonzero` opts out. `require-exit 0` is
/// still accepted (it now just affirms the default).
fn parse_step_body(node: &KdlNode, id: String) -> anyhow::Result<RunStep> {
    let ch = node
        .children()
        .ok_or_else(|| anyhow::anyhow!("run step '{id}' is empty"))?;
    let mut workspace = None;
    let mut command = None;
    let mut env = BTreeMap::new();
    let mut unset = Vec::new();
    let mut retry = None;
    let mut allow_nonzero = false;
    for c in ch.nodes() {
        match c.name().value() {
            "workspace" => workspace = arg(c),
            "command" => command = arg(c),
            "env" => env = parse_env(c),
            "unset" => {
                unset = c
                    .entries()
                    .iter()
                    .filter(|e| e.name().is_none())
                    .filter_map(|e| e.value().as_string().map(String::from))
                    .collect();
            }
            "retry" => retry = Some(parse_retry(c, &id)?),
            // Opt-out: a non-zero exit is expected/fine for this step.
            "allow-nonzero" => allow_nonzero = true,
            // Back-compat: exit 0 is now the DEFAULT, so `require-exit 0` is a redundant affirmation
            // (accepted, no-op). Any other value is still an error.
            "require-exit" => match c.get(0).and_then(|v| v.as_integer()) {
                Some(0) => {}
                Some(n) => anyhow::bail!(
                    "step '{id}': `require-exit {n}` — exit 0 is the default; use `allow-nonzero` to accept a non-zero exit"
                ),
                None => anyhow::bail!(
                    "step '{id}': require-exit needs the integer `0` (or drop it — exit 0 is the default)"
                ),
            },
            other => anyhow::bail!(
                "step '{id}': unexpected node '{other}' (expected workspace|command|env|unset|retry|allow-nonzero)"
            ),
        }
    }
    Ok(RunStep {
        workspace,
        command: command.ok_or_else(|| anyhow::anyhow!("run step '{id}' has no command"))?,
        env,
        unset,
        retry,
        allow_nonzero,
        id,
    })
}

/// `retry { attempts N; interval T }` → a [`Restart`] (mode=fail; `delay` = the inter-retry backoff).
fn parse_retry(node: &KdlNode, step_id: &str) -> anyhow::Result<Restart> {
    let ch = node
        .children()
        .ok_or_else(|| anyhow::anyhow!("step '{step_id}': retry{{}} is empty"))?;
    let mut attempts = None;
    let mut interval = None;
    for c in ch.nodes() {
        match c.name().value() {
            "attempts" => {
                attempts = arg_u32(c);
                if attempts.is_none_or(|n| n == 0) {
                    anyhow::bail!("step '{step_id}': retry attempts needs a positive integer");
                }
            }
            "interval" => {
                let s = arg(c).ok_or_else(|| {
                    anyhow::anyhow!("step '{step_id}': retry interval needs a duration")
                })?;
                interval = Some(
                    parse_duration(&s)
                        .map_err(|e| anyhow::anyhow!("step '{step_id}': retry interval: {e}"))?,
                );
            }
            other => anyhow::bail!(
                "step '{step_id}': retry has unexpected node '{other}' (expected attempts|interval)"
            ),
        }
    }
    let attempts =
        attempts.ok_or_else(|| anyhow::anyhow!("step '{step_id}': retry needs `attempts N`"))?;
    let interval =
        interval.ok_or_else(|| anyhow::anyhow!("step '{step_id}': retry needs `interval T`"))?;
    // Retry maps to a fail-mode Restart: stop after `attempts`, wait `interval` (the backoff) between.
    Ok(Restart {
        attempts,
        interval,
        delay: interval,
        mode: RestartMode::Fail,
    })
}

/// Parse an `env { }` block into a map of var → value (raw; `$CATALOG` expands at spawn).
fn parse_env(node: &KdlNode) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(ch) = node.children() {
        for c in ch.nodes() {
            if let Some(v) = arg(c) {
                env.insert(c.name().value().to_string(), v);
            }
        }
    }
    env
}

/// Merge `parent` and `child` env, child winning. (The env cascade: top → team → agent → process.)
fn cascade(
    parent: &BTreeMap<String, String>,
    child: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = parent.clone();
    out.extend(child.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

// ── Future-collapse helpers (kept expanded in the spec for now; the collapse is unit-tested) ──────

/// The parsed marker for a generated DING sidecar. Task compilation replaces its descriptive command
/// with direct argv bound to the running st2 executable, the projected canonical bus identity, and
/// the effective `ST_ROOT` resolved against `$CATALOG`.
pub fn ding_exec(agent_id: &str) -> SpecExec {
    SpecExec {
        id: format!("{agent_id}.ding"),
        // Identity-only: `st2 ding` defaults its poke target to `--identity` (an agent IS its pty), so
        // the redundant positional is dropped.
        command: format!("st2 ding --identity {agent_id} --root $ST_ROOT"),
        env: BTreeMap::new(),
        derived: true,
    }
}

// ── Parser ───────────────────────────────────────────────────────────────────────────────────────

/// Parse an st2 spec (`.kdl` text) into the [`Spec`] model.
pub fn parse_spec(text: &str) -> anyhow::Result<Spec> {
    let doc = KdlDocument::parse(text).map_err(|e| anyhow::anyhow!("st2 spec KDL parse: {e}"))?;
    crate::kdl_version::ensure_st2_version(&doc)?;

    let mut host = None;
    let mut top_env = BTreeMap::new();
    let mut agents = Vec::new();
    let mut eval = None;

    for node in doc.nodes() {
        match node.name().value() {
            "version" => {}
            "host" => host = arg(node),
            "env" => top_env = parse_env(node),
            "team" => collect_team(node, "", &top_env, &mut agents)?,
            "agent" => agents.push(parse_agent(node, "", &top_env)?),
            "eval" => eval = Some(parse_eval(node, &top_env)?),
            other => {
                anyhow::bail!(
                    "st2 spec: unexpected top-level node '{other}' (expected version|host|env|team|agent|eval)"
                )
            }
        }
    }
    // Re-fold: agents parsed before `env` would miss it, so parse env first. Enforce order simply:
    // if any agent has empty env but a top-level env exists, it means `env` came after — reject for
    // legibility (declare env at the top).
    if eval.as_ref().is_some_and(|eval| eval.canonical_agents)
        && (!agents.is_empty() || eval.as_ref().is_some_and(|eval| !eval.agents.is_empty()))
    {
        anyhow::bail!(
            "eval `canonical-agents` is mutually exclusive with compact `team` / `agent` declarations"
        );
    }
    Ok(Spec {
        host,
        env: top_env,
        agents,
        eval,
    })
}

/// Recurse a `team "name" { }`: prefix its agents' ids and cascade its env into them.
fn collect_team(
    node: &KdlNode,
    prefix: &str,
    parent_env: &BTreeMap<String, String>,
    out: &mut Vec<SpecAgent>,
) -> anyhow::Result<()> {
    let name = arg(node).ok_or_else(|| anyhow::anyhow!("team needs a name"))?;
    let new_prefix = if prefix.is_empty() {
        name
    } else {
        format!("{prefix}.{name}")
    };
    // A team may carry its own env, cascading to the agents it holds.
    let mut team_env = parent_env.clone();
    if let Some(ch) = node.children() {
        for c in ch.nodes() {
            if c.name().value() == "env" {
                team_env = cascade(&team_env, &parse_env(c));
            }
        }
        for c in ch.nodes() {
            match c.name().value() {
                "env" => {}
                "agent" => out.push(parse_agent(c, &new_prefix, &team_env)?),
                "team" => collect_team(c, &new_prefix, &team_env, out)?,
                other => anyhow::bail!(
                    "team '{new_prefix}': unexpected node '{other}' (expected env|agent|team)"
                ),
            }
        }
    }
    Ok(())
}

/// Parse one `agent "id" { }` with the accumulated (top+team) env as its parent scope.
fn parse_agent(
    node: &KdlNode,
    prefix: &str,
    parent_env: &BTreeMap<String, String>,
) -> anyhow::Result<SpecAgent> {
    let name = arg(node).ok_or_else(|| anyhow::anyhow!("agent needs an id"))?;
    let id = if prefix.is_empty() {
        name
    } else {
        format!("{prefix}.{name}")
    };

    let mut display_name = None;
    let mut description = None;
    let mut workspace = None;
    let mut supervisor = None;
    let mut agent_env = BTreeMap::new();
    let mut command = None;
    let mut driver = None;
    let mut execs = Vec::new();
    let mut has_ding = false;

    if let Some(ch) = node.children() {
        // env first so it cascades to execs regardless of source order.
        for c in ch.nodes() {
            if c.name().value() == "env" {
                agent_env = cascade(&agent_env, &parse_env(c));
            }
        }
        let env = cascade(parent_env, &agent_env);
        for c in ch.nodes() {
            match c.name().value() {
                "env" => {}
                "name" => parse_agent_presentation(c, &id, "name", &mut display_name)?,
                "description" => parse_agent_presentation(c, &id, "description", &mut description)?,
                "workspace" => workspace = arg(c),
                "supervisor" => supervisor = arg(c),
                "command" => {
                    anyhow::ensure!(command.is_none(), "agent '{id}' has duplicate `command`");
                    command = Some(arg(c).ok_or_else(|| {
                        anyhow::anyhow!("agent '{id}' `command` must contain a string")
                    })?);
                }
                "claude" | "codex" => {
                    anyhow::ensure!(
                        driver.is_none(),
                        "agent '{id}' declares more than one native driver"
                    );
                    driver = Some(parse_native_driver(c)?);
                }
                "harness" => {
                    anyhow::ensure!(
                        driver.is_none(),
                        "agent '{id}' declares more than one native driver"
                    );
                    let provider = arg(c)
                        .ok_or_else(|| anyhow::anyhow!("agent '{id}' harness needs a provider"))?;
                    anyhow::ensure!(
                        matches!(provider.as_str(), "claude" | "codex"),
                        "agent '{id}' harness '{provider}' is not supported in evals"
                    );
                    anyhow::ensure!(
                        c.entries()
                            .iter()
                            .filter(|entry| entry.name().is_none())
                            .count()
                            == 1,
                        "agent '{id}' harness needs exactly one provider"
                    );
                    let mut provider_node = c.clone();
                    provider_node.set_name(provider);
                    provider_node.entries_mut().clear();
                    driver = Some(parse_native_driver(&provider_node)?);
                }
                // Feature (a): a DEDICATED built-in sidecar node. Bare `ding` expands to the standard
                // st2 ding command (identity/label auto-derived as this agent's id, `--root $ST_ROOT`).
                // It is distinct from `exec` on purpose — `exec` never reserves a magic name, so a
                // command literally named "ding" stays runnable (`exec "ding" { command … }`).
                // (Renames to bare `ping` later.)
                "ding" => {
                    has_ding = true;
                    let mut d = ding_exec(&id);
                    let mut d_env = BTreeMap::new();
                    if let Some(dc) = c.children() {
                        for e in dc.nodes() {
                            if e.name().value() == "env" {
                                d_env = cascade(&d_env, &parse_env(e));
                            }
                        }
                    }
                    d.env = cascade(&env, &d_env);
                    execs.push(d);
                }
                // Feature (b): a plain command process, with AUTO-PREFIXED labels. An exec's leaf name
                // is implicitly scoped to its agent block, so `exec "foo"` in agent `ts.cos` resolves to
                // id `ts.cos.foo`. An already-fully-qualified name (the older verbose form) is kept
                // as-is, so both forms resolve to the same id. A command is always required.
                "exec" => {
                    let ex_leaf =
                        arg(c).ok_or_else(|| anyhow::anyhow!("agent '{id}': exec needs an id"))?;
                    let ex_id = if ex_leaf == id || ex_leaf.starts_with(&format!("{id}.")) {
                        ex_leaf.clone()
                    } else {
                        format!("{id}.{ex_leaf}")
                    };
                    let ex_command = child_arg(c, "command").ok_or_else(|| {
                        anyhow::anyhow!("agent '{id}': exec '{ex_id}' needs a command")
                    })?;
                    let mut ex_env = BTreeMap::new();
                    if let Some(exc) = c.children() {
                        for e in exc.nodes() {
                            if e.name().value() == "env" {
                                ex_env = cascade(&ex_env, &parse_env(e));
                            }
                        }
                    }
                    execs.push(SpecExec {
                        id: ex_id,
                        command: ex_command,
                        env: cascade(&env, &ex_env),
                        derived: false,
                    });
                }
                other => anyhow::bail!(
                    "agent '{id}': unexpected node '{other}' (expected name|description|workspace|supervisor|env|command|harness|claude|codex|ding|exec)"
                ),
            }
        }
        anyhow::ensure!(
            command.is_some() ^ driver.is_some(),
            "agent '{id}' must declare exactly one launch source: `command` or a native `harness`"
        );
        anyhow::ensure!(
            driver.is_none() || !has_ding,
            "agent '{id}' uses a native driver; remove `ding` because the driver owns message delivery"
        );
        validate_presentation("name", display_name.as_deref(), AGENT_NAME_MAX_CHARS)?;
        validate_presentation(
            "description",
            description.as_deref(),
            AGENT_DESCRIPTION_MAX_CHARS,
        )?;
        let env = cascade(parent_env, &agent_env);
        return Ok(SpecAgent {
            id: id.clone(),
            name: display_name,
            description,
            workspace,
            supervisor,
            env,
            command,
            driver,
            execs,
        });
    }
    anyhow::bail!("agent '{id}' is empty")
}

/// Parse the `eval { }` block.
fn parse_eval(node: &KdlNode, top_env: &BTreeMap<String, String>) -> anyhow::Result<Eval> {
    let ch = node
        .children()
        .ok_or_else(|| anyhow::anyhow!("eval {{}} is empty"))?;
    let mut copy = None;
    let mut message = None;
    let mut max_timeout = None;
    let mut agents = Vec::new();
    let mut run_steps = Vec::new();
    let mut judges = Vec::new();
    let mut supervise = false;
    let mut canonical_agents = false;

    for c in ch.nodes() {
        match c.name().value() {
            "copy" => copy = arg(c),
            "message" => message = Some(parse_message(c)?),
            "max-timeout" => {
                let s =
                    arg(c).ok_or_else(|| anyhow::anyhow!("eval: max-timeout needs a duration"))?;
                max_timeout =
                    Some(parse_duration(&s).map_err(|e| anyhow::anyhow!("eval max-timeout: {e}"))?);
            }
            "agent" => agents.push(parse_agent(c, "", top_env)?),
            "team" => collect_team(c, "", top_env, &mut agents)?,
            // The `run { }` stage (act): its `step` children run before judging. Multiple run blocks
            // simply append their steps in order.
            "run" => parse_run_stage(c, &mut run_steps)?,
            "judges" => judges = parse_judges(c)?,
            // Opt-in: a bare `supervise` directive turns on respawn-from-spec during the run.
            "supervise" => supervise = true,
            // Opt-in: use canonical Agent Specs materialized in the hermetic eval catalog.
            "canonical-agents" => {
                if canonical_agents {
                    anyhow::bail!("eval: `canonical-agents` may occur only once");
                }
                if !c.entries().is_empty() || c.children().is_some() {
                    anyhow::bail!("eval: `canonical-agents` is a bare directive");
                }
                canonical_agents = true;
            }
            other => anyhow::bail!(
                "eval: unexpected node '{other}' (expected copy|message|max-timeout|agent|team|run|judges|supervise|canonical-agents)"
            ),
        }
    }
    // A team eval needs a kickoff; a team-less eval (run steps + judges) has nothing to kick off.
    if message.is_none() && agents.is_empty() && run_steps.is_empty() {
        anyhow::bail!("eval has no message{{}} kickoff and no run{{}} steps — nothing to run");
    }
    Ok(Eval {
        copy,
        message,
        max_timeout: max_timeout.ok_or_else(|| anyhow::anyhow!("eval has no max-timeout"))?,
        agents,
        run_steps,
        judges,
        supervise,
        canonical_agents,
    })
}

/// Parse `message { from "…" to "…" content "…" }` (from/to/content as separate child nodes).
fn parse_message(node: &KdlNode) -> anyhow::Result<Kick> {
    // Catch the reference-example glitch first: `message { from "r" to "a" content "c" }` on one line
    // (no newline/`;` separators) mis-parses as a SINGLE `from` node with the rest as args. Reject it
    // with a fix-it rather than silently taking one field.
    if node.children().is_some_and(|d| {
        d.nodes()
            .iter()
            .any(|n| n.name().value() == "from" && n.entries().len() > 1)
    }) {
        anyhow::bail!(
            "message: `from`/`to`/`content` must be SEPARATE nodes (newline- or `;`-separated), not one \
             unseparated line — `message {{ from \"r\"; to \"a\"; content \"./task.md\" }}`"
        );
    }
    let from = child_arg(node, "from").ok_or_else(|| anyhow::anyhow!("message needs `from`"))?;
    let to = child_arg(node, "to").ok_or_else(|| anyhow::anyhow!("message needs `to`"))?;
    let content =
        child_arg(node, "content").ok_or_else(|| anyhow::anyhow!("message needs `content`"))?;
    Ok(Kick { from, to, content })
}

/// Parse the `judges { }` block into all-must-pass judges.
fn parse_judges(node: &KdlNode) -> anyhow::Result<Vec<Judge>> {
    let mut judges = Vec::new();
    let Some(ch) = node.children() else {
        return Ok(judges);
    };
    for c in ch.nodes() {
        if c.name().value() != "judge" {
            anyhow::bail!(
                "judges: unexpected node '{}' (expected judge)",
                c.name().value()
            );
        }
        judges.push(parse_judge(c)?);
    }
    Ok(judges)
}

/// Parse one `judge "name" { … }` — exactly one flavor (declarative | bash | ask) + optional timeout.
fn parse_judge(node: &KdlNode) -> anyhow::Result<Judge> {
    let name = arg(node).ok_or_else(|| anyhow::anyhow!("judge needs a name"))?;
    let ch = node
        .children()
        .ok_or_else(|| anyhow::anyhow!("judge '{name}' is empty"))?;

    let mut timeout = None;
    let mut exec_cmd = None;
    let mut ask = None;
    let mut checks = Vec::new();
    let mut signal = false;

    for c in ch.nodes() {
        match c.name().value() {
            // Opt-in: a bare `signal` directive → show-but-don't-gate (excluded from SCORE/verdict).
            "signal" => signal = true,
            "timeout" => {
                if let Some(s) = arg(c) {
                    timeout = Some(
                        parse_duration(&s)
                            .map_err(|e| anyhow::anyhow!("judge '{name}' timeout: {e}"))?,
                    );
                }
            }
            "exec" => exec_cmd = arg(c),
            "ask" => {
                let agent =
                    arg(c).ok_or_else(|| anyhow::anyhow!("judge '{name}': ask needs an agent"))?;
                let prompt = arg_n(c, 1)
                    .ok_or_else(|| anyhow::anyhow!("judge '{name}': ask needs a prompt"))?;
                ask = Some((agent, prompt));
            }
            "file" => {
                let path =
                    arg(c).ok_or_else(|| anyhow::anyhow!("judge '{name}': file needs a path"))?;
                let op = arg_n(c, 1)
                    .ok_or_else(|| anyhow::anyhow!("judge '{name}': file needs has/lacks"))?;
                let text = arg_n(c, 2)
                    .ok_or_else(|| anyhow::anyhow!("judge '{name}': file needs a value"))?;
                checks.push(match op.as_str() {
                    "has" => Check::FileHas { path, text },
                    "lacks" => Check::FileLacks { path, text },
                    other => {
                        anyhow::bail!("judge '{name}': file op '{other}' (expected has|lacks)")
                    }
                });
            }
            "json" => {
                // json "p" field "x" is "y"
                let path =
                    arg(c).ok_or_else(|| anyhow::anyhow!("judge '{name}': json needs a path"))?;
                let field = arg_n(c, 2)
                    .ok_or_else(|| anyhow::anyhow!("judge '{name}': json needs `field <x>`"))?;
                let value = c
                    .get(4)
                    .and_then(|v| match v {
                        KdlValue::String(s) => Some(JsonScalar::String(s.clone())),
                        KdlValue::Bool(b) => Some(JsonScalar::Bool(*b)),
                        KdlValue::Integer(i) => (*i).try_into().ok().map(JsonScalar::Integer),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "judge '{name}': json value must be string, boolean, or integer"
                        )
                    })?;
                checks.push(Check::JsonField { path, field, value });
            }
            "committed" => {
                let path = arg(c)
                    .ok_or_else(|| anyhow::anyhow!("judge '{name}': committed needs a path"))?;
                checks.push(Check::Committed { path });
            }
            other => anyhow::bail!("judge '{name}': unexpected node '{other}'"),
        }
    }

    // Exactly one flavor.
    let flavors = [exec_cmd.is_some(), ask.is_some(), !checks.is_empty()]
        .iter()
        .filter(|b| **b)
        .count();
    if flavors == 0 {
        anyhow::bail!(
            "judge '{name}' has no check (need exec | ask | declarative file/json/committed)"
        );
    }
    if flavors > 1 {
        anyhow::bail!(
            "judge '{name}' mixes flavors — a judge is exactly one of exec | ask | declarative"
        );
    }
    let kind = if let Some(cmd) = exec_cmd {
        JudgeKind::Bash(cmd)
    } else if let Some((agent, prompt)) = ask {
        JudgeKind::Ask { agent, prompt }
    } else {
        JudgeKind::Declarative(checks)
    };
    Ok(Judge {
        name,
        timeout,
        kind,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The reference example (design: license-mit), with the two KDL-validity fixes I proved against
    // kdl-6: `#`-comments → `//`, and the `message` fields separated (they mis-parse on one line).
    // An explicit custom bus root proves that authored ST_ROOT overrides are preserved.
    const REFERENCE: &str = r##"
env {
  ST_ROOT  "$CATALOG/custom-bus"
  PTY_ROOT "$CATALOG/pty"
}

team "mix" {
  agent "sup" {
    workspace "./sup"
    command #"exec claude --permission-mode bypassPermissions 'boot: sup'"#
    ding                                 // built-in sidecar (auto-prefixed to mix.sup.ding, --root $ST_ROOT)
  }
  agent "worker" {
    workspace "./worker"
    command #"exec claude --permission-mode auto 'boot: worker'"#
    ding
  }
}

eval {
  copy "./fixture"
  message { from "requester"; to "mix.sup"; content "./task.md" }
  max-timeout "1200s"

  agent "judge" {
    workspace "./"
    command #"exec codex --model gpt-5.6-sol 'boot: judge'"#
    ding                                 // built-in sidecar (auto-prefixed to judge.ding)
  }

  judges {
    judge "isolation" { exec "sh ./judges/isolation.sh" }
    judge "LICENSE is real MIT" {
      file "worker/LICENSE" has "Permission is hereby granted, free of charge"
      file "worker/LICENSE" lacks "proprietary"
    }
    judge "package.json says MIT" {
      json "worker/package.json" field "license" is "MIT"
    }
    judge "committed + clean" { exec "sh ./judges/committed.sh" }
    judge "confirmation is verified, not a rubber-stamp" {
      timeout "120s"
      ask "judge" #"Read the confirmation. Reply: PASS or FAIL, then one sentence."#
    }
  }
}
"##;

    #[test]
    fn parses_the_reference_example() {
        let s = parse_spec(REFERENCE).unwrap();
        // Top-level env cascades.
        assert_eq!(s.env.get("ST_ROOT").unwrap(), "$CATALOG/custom-bus");
        // Team prefixes agent ids.
        let ids: Vec<&str> = s.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["mix.sup", "mix.worker"]);
        // Agent IS its pty (command on the agent) + cascaded authored env. The runner adds identity.
        let sup = &s.agents[0];
        assert_eq!(sup.workspace.as_deref(), Some("./sup"));
        assert!(
            sup.command
                .as_deref()
                .unwrap()
                .contains("exec claude --permission-mode bypassPermissions")
        );
        assert_eq!(sup.env.get("ST_ROOT").unwrap(), "$CATALOG/custom-bus"); // cascaded from top
        assert!(!sup.env.contains_key("ST_AGENT"));
        // The bare `ding` node auto-derives `<agent>.ding` and st2 generates the standard sidecar
        // against the cascaded $ST_ROOT; it inherits the agent env.
        assert_eq!(sup.execs.len(), 1);
        assert_eq!(sup.execs[0].id, "mix.sup.ding");
        assert_eq!(
            sup.execs[0].command,
            "st2 ding --identity mix.sup --root $ST_ROOT"
        );
        assert!(!sup.execs[0].env.contains_key("ST_AGENT"));

        // Eval block.
        let ev = s.eval.as_ref().unwrap();
        assert_eq!(ev.copy.as_deref(), Some("./fixture"));
        assert_eq!(
            ev.message,
            Some(Kick {
                from: "requester".into(),
                to: "mix.sup".into(),
                content: "./task.md".into()
            })
        );
        assert_eq!(ev.max_timeout, Duration::from_secs(1200));
        assert_eq!(ev.agents.len(), 1); // the eval-only judge agent
        assert_eq!(ev.agents[0].id, "judge");

        // Judges — all three flavors parsed.
        assert_eq!(ev.judges.len(), 5);
        assert!(
            matches!(&ev.judges[0].kind, JudgeKind::Bash(c) if c == "sh ./judges/isolation.sh")
        );
        match &ev.judges[1].kind {
            JudgeKind::Declarative(checks) => {
                assert_eq!(checks.len(), 2);
                assert!(matches!(&checks[0], Check::FileHas { path, text }
                    if path == "worker/LICENSE" && text.starts_with("Permission is hereby")));
                assert!(
                    matches!(&checks[1], Check::FileLacks { path, .. } if path == "worker/LICENSE")
                );
            }
            other => panic!("expected declarative, got {other:?}"),
        }
        match &ev.judges[2].kind {
            JudgeKind::Declarative(checks) => assert!(matches!(&checks[0],
                Check::JsonField { path, field, value } if path == "worker/package.json" && field == "license" && *value == JsonScalar::String("MIT".into()))),
            other => panic!("expected json declarative, got {other:?}"),
        }
        // Per-judge timeout + ask flavor.
        assert_eq!(ev.judges[4].timeout, Some(Duration::from_secs(120)));
        assert!(matches!(&ev.judges[4].kind, JudgeKind::Ask { agent, .. } if agent == "judge"));
    }

    #[test]
    fn parses_native_claude_and_codex_eval_agents() {
        let spec = parse_spec(
            r#"
team "mix" {
  agent "sup" {
    workspace "./sup"
    claude {
      model "claude-sonnet-5"
      effort "medium"
      prompt "Coordinate the task."
      args "--permission-mode" "bypassPermissions"
    }
  }
}
eval {
  message { from "requester"; to "mix.sup"; content "work" }
  max-timeout "60s"
  agent "judge" {
    codex {
      model "gpt-5.6-sol"
      effort "medium"
      prompt "Judge the result."
    }
  }
}
"#,
        )
        .unwrap();

        assert!(spec.agents[0].command.is_none());
        assert!(matches!(
            spec.agents[0].driver.as_ref(),
            Some(Driver::Claude(ClaudeDriver {
                model: Some(model),
                effort: Some(effort),
                prompt,
                args,
                ..
            })) if model == "claude-sonnet-5"
                && effort == "medium"
                && prompt == "Coordinate the task."
                && args == &["--permission-mode", "bypassPermissions"]
        ));
        let judge = &spec.eval.unwrap().agents[0];
        assert!(matches!(
            judge.driver.as_ref(),
            Some(Driver::Codex(CodexDriver {
                model: Some(model),
                effort: Some(effort),
                prompt,
                ..
            })) if model == "gpt-5.6-sol"
                && effort == "medium"
                && prompt == "Judge the result."
        ));
    }

    #[test]
    fn parses_contextual_harness_eval_agents() {
        let spec = parse_spec(
            r#"
agent "worker" {
  harness "claude" {
    model "claude-sonnet-5"
    effort "medium"
    prompt "Do the work."
    args "--permission-mode" "bypassPermissions"
  }
}
eval {
  message { from "requester"; to "worker"; content "work" }
  max-timeout "60s"
  agent "judge" {
    harness "codex" {
      model "gpt-5.6-sol"
      effort "medium"
      prompt "Judge the work."
    }
  }
}
"#,
        )
        .unwrap();

        assert!(matches!(
            spec.agents[0].driver.as_ref(),
            Some(Driver::Claude(ClaudeDriver { prompt, .. })) if prompt == "Do the work."
        ));
        assert!(matches!(
            spec.eval.unwrap().agents[0].driver.as_ref(),
            Some(Driver::Codex(CodexDriver { prompt, .. })) if prompt == "Judge the work."
        ));
    }

    #[test]
    fn native_eval_agents_reject_ambiguous_launch_and_ding() {
        for source in [
            r#"agent "a" { command "true"; claude { prompt "work" } }"#,
            r#"agent "a" { claude { prompt "work" }; codex { prompt "work" } }"#,
            r#"agent "a" { claude { prompt "work" }; ding }"#,
        ] {
            assert!(
                parse_spec(source).is_err(),
                "source unexpectedly parsed: {source}"
            );
        }
    }

    #[test]
    fn bare_ding_node_generates_the_builtin_sidecar() {
        // The dedicated `ding` node must parse to EXACTLY what the `ding_exec` helper generates.
        let sup = &parse_spec(REFERENCE).unwrap().agents[0];
        assert!(sup.execs[0].derived);
        assert_eq!(
            ding_exec("mix.sup"),
            SpecExec {
                id: sup.execs[0].id.clone(),
                command: sup.execs[0].command.clone(),
                env: BTreeMap::new(),
                derived: true,
            }
        );
    }

    #[test]
    fn compact_agents_lower_bounded_presentation_without_changing_identity() {
        let spec = parse_spec(
            r#"agent "worker" {
  name "Build owner"
  description "Own build delivery"
  command "true"
}"#,
        )
        .unwrap();
        let agent = &spec.agents[0];
        assert_eq!(agent.id, "worker");
        assert_eq!(agent.name.as_deref(), Some("Build owner"));
        assert_eq!(agent.description.as_deref(), Some("Own build delivery"));

        for field in ["name", "description"] {
            for separator in ['\u{2028}', '\u{2029}'] {
                let source = format!(
                    "agent \"worker\" {{ {field} \"left{separator}right\"; command \"true\" }}"
                );
                assert!(
                    parse_spec(&source).is_err(),
                    "accepted {field} U+{:04X}",
                    separator as u32
                );
            }
            let duplicate = format!(
                "agent \"worker\" {{ {field} \"one\"; {field} \"two\"; command \"true\" }}"
            );
            assert!(
                parse_spec(&duplicate).is_err(),
                "accepted duplicate {field}"
            );

            let malformed = format!("agent \"worker\" {{ {field} 1; command \"true\" }}");
            assert!(
                parse_spec(&malformed).is_err(),
                "accepted malformed {field}"
            );
        }
    }

    #[test]
    fn verbose_full_label_exec_still_parses_without_double_prefix() {
        // Back-compat: the older fully-qualified exec label resolves to the same id (no double-prefix).
        let s = parse_spec(
            r#"
team "mix" {
  agent "sup" {
    command "boot"
    exec "mix.sup.sidecar" { command "custom cmd" }
  }
}
"#,
        )
        .unwrap();
        let ex = &s.agents[0].execs[0];
        assert_eq!(ex.id, "mix.sup.sidecar");
        assert_eq!(ex.command, "custom cmd");
    }

    #[test]
    fn exec_named_ding_is_a_plain_runnable_command() {
        // The whole point of the dedicated `ding` node: `exec` reserves NO magic name, so a command
        // literally named "ding" is runnable — `exec "ding"` is a normal exec, NOT the built-in sidecar.
        let s = parse_spec(
            r#"
team "mix" {
  agent "sup" { command "boot"; exec "ding" { command "ding --ring-the-bell" } }
}
"#,
        )
        .unwrap();
        let ex = &s.agents[0].execs[0];
        assert_eq!(ex.id, "mix.sup.ding");
        assert_eq!(ex.command, "ding --ring-the-bell"); // the literal command, not the st2 template
        assert!(!ex.derived);
    }

    #[test]
    fn non_ding_exec_leaf_is_auto_prefixed_and_needs_a_command() {
        // A non-ding leaf is auto-prefixed to `<agent>.<leaf>` and still requires an explicit command.
        let s = parse_spec(
            r#"
team "mix" {
  agent "sup" { command "boot"; exec "sidecar" { command "run it" } }
}
"#,
        )
        .unwrap();
        let ex = &s.agents[0].execs[0];
        assert_eq!(ex.id, "mix.sup.sidecar");
        assert_eq!(ex.command, "run it");

        // ...and a command-less non-ding exec is an error (only "ding" has a built-in).
        let err = parse_spec(r#"team "mix" { agent "sup" { command "boot"; exec "sidecar" } }"#)
            .unwrap_err();
        assert!(err.to_string().contains("needs a command"), "{err}");
    }

    #[test]
    fn json_judge_rejects_non_scalar_expected_values() {
        for value in ["null", "[1]", "{\"x\":1}", "1.5"] {
            let text = format!(
                "eval {{ judges {{ judge \"j\" {{ json \"x.json\" field \"n\" is {value} }} }} }}"
            );
            assert!(
                parse_spec(&text).is_err(),
                "accepted invalid JSON scalar {value}"
            );
        }
    }

    #[test]
    fn parses_a_top_level_host_and_defaults_to_none() {
        // A per-host fleet file declares the logical host at the top. Absent → None (falls back to
        // --host / OS hostname).
        let with = parse_spec("host \"silber\"\nagent \"a\" { command \"x\" }").unwrap();
        assert_eq!(with.host.as_deref(), Some("silber"));
        let without = parse_spec("agent \"a\" { command \"x\" }").unwrap();
        assert_eq!(without.host, None);
    }

    #[test]
    fn eval_supervise_flag_is_opt_in() {
        // A bare `supervise` directive in the eval block turns on respawn-from-spec; absent → boot-once.
        let ev = |extra: &str| {
            format!(
                "agent \"sup\" {{ command \"x\" }}\n\
                 eval {{\n  message {{ from \"r\"; to \"sup\"; content \"go\" }}\n  max-timeout \"5s\"\n{extra}}}\n"
            )
        };
        assert!(
            parse_spec(&ev("  supervise\n"))
                .unwrap()
                .eval
                .unwrap()
                .supervise
        );
        assert!(!parse_spec(&ev("")).unwrap().eval.unwrap().supervise);
    }

    #[test]
    fn canonical_agents_is_bare_once_and_excludes_compact_agents() {
        let canonical = parse_spec(
            "eval {\n  canonical-agents\n  message { from \"r\"; to \"h.sup\"; content \"go\" }\n  max-timeout \"5s\"\n}\n",
        )
        .unwrap();
        assert!(canonical.eval.unwrap().canonical_agents);

        for invalid in [
            "eval { canonical-agents \"./fleet\"; max-timeout \"5s\"; run \"x\" { command \"true\" } }",
            "eval { canonical-agents { }; max-timeout \"5s\"; run \"x\" { command \"true\" } }",
            "eval { canonical-agents; canonical-agents; max-timeout \"5s\"; run \"x\" { command \"true\" } }",
            "agent \"a\" { command \"true\" }\neval { canonical-agents; message { from \"r\"; to \"a\"; content \"go\" }; max-timeout \"5s\" }",
            "eval { canonical-agents; agent \"a\" { command \"true\" }; message { from \"r\"; to \"a\"; content \"go\" }; max-timeout \"5s\" }",
        ] {
            assert!(
                parse_spec(invalid).is_err(),
                "accepted ambiguous canonical agent authority:\n{invalid}"
            );
        }
    }

    #[test]
    fn judge_signal_flag_is_opt_in() {
        // A bare `signal` directive marks a judge show-but-don't-gate; absent → a normal gating judge.
        let spec = parse_spec(
            "eval {\n  max-timeout \"5s\"\n  run \"s\" { command \"true\" }\n  \
             judges { judge \"gate\" { exec \"true\" }\n            judge \"sig\" { signal; exec \"true\" } }\n}\n",
        )
        .unwrap();
        let judges = spec.eval.unwrap().judges;
        assert_eq!(judges.len(), 2);
        assert!(!judges[0].signal, "a plain judge gates");
        assert!(judges[1].signal, "a `signal` judge does not gate");
    }

    #[test]
    fn parses_a_team_less_eval_with_flat_run_steps() {
        // A file with NO agents + run steps is a team-less eval — no kickoff message required. FLAT
        // run-collapse form: each `run "label" { … }` IS one step; multiple `run` nodes run in order.
        let spec = parse_spec(
            "eval {\n  max-timeout \"2m\"\n  \
             run \"build\" { workspace \"repo\"; command \"make check\"; retry { attempts 3; interval \"5s\" } }\n  \
             run \"probe\" { command \"doctor\"; env { FOO \"bar\" }; unset \"ST_ROOT\" \"PTY_ROOT\"; allow-nonzero }\n  \
             judges { judge \"ok\" { exec \"true\" } }\n}\n",
        )
        .unwrap();
        assert!(spec.agents.is_empty(), "team-less: no base-team agents");
        let ev = spec.eval.unwrap();
        assert!(
            ev.message.is_none(),
            "a team-less eval has no kickoff message"
        );
        assert_eq!(
            ev.run_steps.len(),
            2,
            "two flat run nodes → two ordered steps"
        );
        let build = &ev.run_steps[0];
        assert_eq!(build.id, "build");
        assert_eq!(build.workspace.as_deref(), Some("repo"));
        assert_eq!(build.command, "make check");
        assert!(!build.allow_nonzero, "default is must-exit-0");
        let r = build.retry.as_ref().unwrap();
        assert_eq!(r.attempts, 3);
        assert_eq!(r.delay, Duration::from_secs(5)); // interval → the inter-retry backoff
        let probe = &ev.run_steps[1];
        assert_eq!(probe.id, "probe");
        assert_eq!(probe.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(
            probe.unset,
            vec!["ST_ROOT".to_string(), "PTY_ROOT".to_string()]
        );
        assert!(probe.allow_nonzero, "opted out of must-exit-0");
    }

    #[test]
    fn nested_run_step_form_is_rejected() {
        // The old `run { step … }` wrapper was retired: a `run` with no label is now an error
        // (every eval uses the flat `run "label" { … }` form).
        let err = parse_spec(
            "eval {\n  max-timeout \"2m\"\n  run {\n    step \"render\" { command \"x\" }\n  }\n  \
             judges { judge \"ok\" { exec \"true\" } }\n}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("run needs a label"), "{err}");
    }

    #[test]
    fn env_cascade_child_overrides_parent() {
        let s = parse_spec(
            r#"
            env { A "top"; B "top" }
            agent "x" { env { B "agent" }; command "run" }
            "#,
        )
        .unwrap();
        let x = &s.agents[0];
        assert_eq!(x.env.get("A").unwrap(), "top"); // inherited
        assert_eq!(x.env.get("B").unwrap(), "agent"); // overridden
    }

    #[test]
    fn nested_teams_prefix_dotted() {
        let s = parse_spec(r#"team "a" { team "b" { agent "c" { command "run" } } }"#).unwrap();
        assert_eq!(s.agents[0].id, "a.b.c");
    }

    #[test]
    fn unseparated_message_line_is_a_loud_error() {
        // The reference example's `message { from "r" to "a" content "c" }` on one line mis-parses as a
        // single `from` node — we reject it with a fix-it message rather than silently taking one field.
        let err = parse_spec(
            r#"eval { copy "./f"; message { from "r" to "mix.sup" content "./t.md" }; max-timeout "1s"; judges { } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("SEPARATE nodes"), "got: {err}");
    }

    #[test]
    fn a_judge_must_be_exactly_one_flavor() {
        let err = parse_spec(
            r#"eval { message { from "r"; to "a"; content "c" }; max-timeout "1s"; judges { judge "bad" { exec "x"; ask "j" "p" } } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one"), "got: {err}");
    }

    #[test]
    fn st2_accepts_versions_zero_and_one_but_rejects_version_two() {
        parse_spec("agent \"zero\" { command \"true\" }").unwrap();
        parse_spec("version 0\nagent \"zero\" { command \"true\" }").unwrap();
        parse_spec("version 1\nagent \"one\" { command \"true\" }").unwrap();
        let error = parse_spec("version 2\nagent \"two\" { command \"true\" }").unwrap_err();
        assert!(error.to_string().contains("st2 accepts KDL version 0 or 1"));
    }
}
