//! Reconcile — the declarative core (VRS R02). Computes **DESIRED** (discovered `service` specs,
//! host-filtered to this machine) vs **ACTUAL** (live task sessions/processes) and returns a plan:
//! which tasks to launch, tear down (retired), adopt (running), skip (other host), and GC.
//!
//! Pure and side-effect-free, so it is exhaustively unit-testable; execution lives behind backends
//! (the `pty` CLI for `pty` tasks, direct process supervision for terminal-free `exec` tasks). st2
//! reconciles at the **task** level — explicitly authored sibling tasks remain independent. A
//! generated companion is instead eligible only with its canonical agent task and is stopped when
//! that target is held or terminally parked.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use agent_spec::spec::{
    AgentSpec, DeliveryTransport, Driver, TaskKind, TaskLifecycle, stream_name_of_task,
};
use kdl::KdlValue;

/// Immutable inputs captured once before generated tasks are compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompileContext {
    catalog_root: PathBuf,
    st2_executable: PathBuf,
}

impl TaskCompileContext {
    pub fn new(catalog_root: PathBuf, st2_executable: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            catalog_root.is_absolute(),
            "task compilation catalog root is not absolute: {}",
            catalog_root.display()
        );
        anyhow::ensure!(
            st2_executable.is_absolute(),
            "task compilation st2 executable is not absolute: {}",
            st2_executable.display()
        );
        anyhow::ensure!(
            st2_executable
                .try_exists()
                .with_context(|| format!("checking st2 executable {}", st2_executable.display()))?,
            "task compilation st2 executable does not exist: {}",
            st2_executable.display()
        );
        anyhow::ensure!(
            st2_executable
                .metadata()
                .with_context(|| format!("reading st2 executable {}", st2_executable.display()))?
                .is_file(),
            "task compilation st2 executable is not a file: {}",
            st2_executable.display()
        );
        Ok(Self {
            catalog_root,
            st2_executable,
        })
    }

    pub fn current(catalog_root: PathBuf) -> Result<Self> {
        let catalog_root = if catalog_root.is_absolute() {
            catalog_root
        } else {
            std::env::current_dir()
                .context("resolving the task compilation catalog root")?
                .join(catalog_root)
        };
        let st2_executable =
            std::env::current_exe().context("resolving the running st2 executable")?;
        Self::new(catalog_root, st2_executable)
    }

    pub fn st2_executable(&self) -> &Path {
        &self.st2_executable
    }

    pub fn catalog_root(&self) -> &Path {
        &self.catalog_root
    }
}

/// Compile every runner-owned launch marker into an exact invocation of this st2 binary.
pub fn compile_generated_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    for spec in specs.iter() {
        crate::driver::ensure_single_source(spec)?;
    }
    compile_driver_agent_tasks(specs, this_host, context)?;
    compile_claude_session_agent_tasks(specs, this_host, context)?;
    compile_pi_session_agent_tasks(specs, this_host, context)?;
    compile_generated_ding_tasks(specs, this_host, context)?;
    compile_app_server_agent_tasks(specs, this_host, context)?;
    // Claude's MCP server remains declared to Claude itself. The canonical task wrapper owns only
    // the provider lifetime and its presence lease. It does not add a supervisor-owned companion.
    Ok(())
}

/// Compile the shared printed driver expansion into the canonical agent task.
pub fn compile_driver_agent_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    let st2_executable = context
        .st2_executable
        .to_str()
        .context("running st2 executable path is not UTF-8")?
        .to_owned();
    let catalog_root = context
        .catalog_root
        .to_str()
        .context("catalog root is not UTF-8")?
        .to_owned();

    for spec in specs {
        let Some(driver) = spec.driver.as_ref() else {
            continue;
        };
        let bus_id = spec.bus_id(this_host);
        let expansion = crate::driver::expand_driver(spec, this_host)?;
        let argv_nodes = expansion
            .nodes()
            .iter()
            .filter(|node| node.name().value() == "argv")
            .collect::<Vec<_>>();
        let [argv_node] = argv_nodes.as_slice() else {
            anyhow::bail!(
                "agent '{bus_id}' driver expansion produced {} argv nodes; expected exactly one",
                argv_nodes.len()
            );
        };
        anyhow::ensure!(
            argv_node.children().is_none()
                && argv_node.entries().iter().all(|entry| {
                    entry.name().is_none() && matches!(entry.value(), KdlValue::String(_))
                }),
            "agent '{bus_id}' driver expansion produced a non-string argv"
        );
        let mut argv = argv_node
            .entries()
            .iter()
            .map(|entry| match entry.value() {
                KdlValue::String(value) => value.clone(),
                _ => unreachable!("the argv shape check accepts only strings"),
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            !argv.is_empty(),
            "agent '{bus_id}' driver expansion produced an empty argv"
        );

        let wrapper = match driver {
            Driver::Codex(_) => "codex",
            Driver::Claude(_) => "claude-session",
            Driver::Pi(_) => "pi-session",
            Driver::OpenCode(_) => "opencode-session",
            Driver::Omp(_) => "omp-session",
        };
        anyhow::ensure!(
            argv.first().map(String::as_str) == Some("st2")
                && argv.get(1).map(String::as_str) == Some("--catalog")
                && argv.get(2).map(String::as_str) == Some("$CATALOG")
                && argv.get(3).map(String::as_str) == Some("driver")
                && argv.get(4).map(String::as_str) == Some(wrapper),
            "agent '{bus_id}' driver expansion has an unexpected {wrapper} wrapper prefix"
        );
        argv[0] = st2_executable.clone();
        argv[2] = catalog_root.clone();

        let mut candidates = spec
            .tasks
            .iter_mut()
            .filter(|task| !task.derived && task.name == "agent");
        let task = candidates
            .next()
            .with_context(|| format!("agent '{bus_id}' driver has no canonical `agent` task"))?;
        anyhow::ensure!(
            candidates.next().is_none(),
            "agent '{bus_id}' driver has more than one canonical `agent` task"
        );
        anyhow::ensure!(
            task.kind == TaskKind::Pty,
            "agent '{bus_id}' driver canonical task is not a PTY"
        );
        task.command = None;
        task.argv = Some(argv);
    }
    Ok(())
}

/// Route legacy MCP delivery through the same Claude session wrapper as a typed driver.
pub fn compile_claude_session_agent_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    compile_session_wrapped_agent_tasks(
        specs,
        this_host,
        context,
        DeliveryTransport::Mcp,
        "claude-session",
    )
}

/// Route legacy pi-channel delivery through the same pi session wrapper as a typed driver.
///
/// pi's channel is an extension the wrapper injects, so — unlike `mcp` — this transport adds
/// nothing to the workspace. A hand-authored pi task therefore needs only the wrapper.
pub fn compile_pi_session_agent_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    compile_session_wrapped_agent_tasks(
        specs,
        this_host,
        context,
        DeliveryTransport::PiChannel,
        "pi-session",
    )
}

/// The shape both session-wrapped transports share: take the one canonical PTY `agent` task's
/// authored launch and re-express it as that provider's st2 wrapper, preserving the runtime id.
fn compile_session_wrapped_agent_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
    transport: DeliveryTransport,
    wrapper: &str,
) -> Result<()> {
    let st2_executable = context
        .st2_executable
        .to_str()
        .context("running st2 executable path is not UTF-8")?
        .to_owned();
    let catalog_root = context
        .catalog_root
        .to_str()
        .context("catalog root is not UTF-8")?
        .to_owned();

    for spec in specs {
        if spec.driver.is_some() || spec.delivery != Some(transport) {
            continue;
        }
        let selected = transport.as_str();
        let bus_id = spec.bus_id(this_host);
        let mut candidates = spec
            .tasks
            .iter_mut()
            .filter(|task| !task.derived && task.name == "agent");
        let task = candidates.next().with_context(|| {
            format!(
                "agent '{bus_id}' selects `deliver \"{selected}\"` but has no canonical `agent` task"
            )
        })?;
        anyhow::ensure!(
            candidates.next().is_none(),
            "agent '{bus_id}' selects `deliver \"{selected}\"` with more than one canonical `agent` task"
        );
        anyhow::ensure!(
            task.kind == TaskKind::Pty,
            "agent '{bus_id}' selects `deliver \"{selected}\"` for a non-PTY canonical task"
        );
        let provider = match (&task.command, &task.argv) {
            (None, Some(argv)) => argv.clone(),
            (Some(command), None) => {
                vec!["sh".to_string(), "-c".to_string(), command.clone()]
            }
            (None, None) => Vec::new(),
            (Some(_), Some(_)) => {
                unreachable!("discovery rejects tasks carrying both command and argv")
            }
        };
        anyhow::ensure!(
            !provider.is_empty(),
            "agent '{bus_id}' selects `deliver \"{selected}\"` with an empty canonical argv"
        );
        let runtime_id = task
            .id
            .clone()
            .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
        let mut argv = vec![
            st2_executable.clone(),
            "--catalog".to_string(),
            catalog_root.clone(),
            "driver".to_string(),
            wrapper.to_string(),
            "--identity".to_string(),
            bus_id,
            "--runtime-id".to_string(),
            runtime_id,
            "--".to_string(),
        ];
        argv.extend(provider);
        task.command = None;
        task.argv = Some(argv);
    }
    Ok(())
}

/// Replace only runner-generated companion markers with exact direct argv. Authored tasks never
/// carry `derived=true`, so source that happens to invoke `st2 ding` or `st2 stream run` remains
/// byte-for-byte unchanged.
///
/// The single `ensure!` below is the fail-closed "unsupported derived task" gate. Every derived
/// companion kind must be named in the exhaustive match; an unrecognized derived task refuses the
/// pass rather than reaching a runner with an unbound placeholder command.
pub fn compile_generated_ding_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    let st2_executable = context
        .st2_executable
        .to_str()
        .context("running st2 executable path is not UTF-8")?
        .to_owned();
    for spec in specs {
        let bus_id = spec.bus_id(this_host);
        for task in &mut spec.tasks {
            if !task.derived {
                continue;
            }
            let is_ding = task.name == "ding" || task.name.ends_with(".ding");
            let task_name = task.name.clone();
            let stream_name = stream_name_of_task(&task_name).map(str::to_owned);
            anyhow::ensure!(
                task.kind == TaskKind::Exec && (is_ding || stream_name.is_some()),
                "unsupported derived task: {}",
                task.name
            );
            let effective_root = task
                .env
                .get("ST_ROOT")
                .map(|root| crate::expand::expand_catalog(root, &context.catalog_root))
                .unwrap_or_else(|| context.catalog_root.display().to_string());
            anyhow::ensure!(
                Path::new(&effective_root).is_absolute(),
                "derived companion root is not absolute: {effective_root}"
            );
            if let Some(stream_name) = stream_name {
                anyhow::ensure!(
                    task.command.is_some() != task.argv.is_some(),
                    "derived stream task '{task_name}' for stream '{stream_name}' has no exact launch"
                );
                continue;
            }
            task.command = None;
            task.argv = Some(vec![
                st2_executable.clone(),
                "ding".to_string(),
                "--identity".to_string(),
                bus_id.clone(),
                "--root".to_string(),
                effective_root,
            ]);
        }
    }
    Ok(())
}

/// Route an explicitly selected Codex native transport through st2's controlled-launch wrapper.
///
/// The wrapper owns the provider daemon and its control connection, so it can complete the
/// initialize handshake before the interactive client is allowed to create or resume a thread.
/// App-server delivery therefore requires structured argv: rewriting opaque shell source would be
/// unsound, and an already-remote launch would have two competing control owners.
pub fn compile_app_server_agent_tasks(
    specs: &mut [AgentSpec],
    this_host: &str,
    context: &TaskCompileContext,
) -> Result<()> {
    let st2_executable = context
        .st2_executable
        .to_str()
        .context("running st2 executable path is not UTF-8")?
        .to_owned();
    let catalog_root = context
        .catalog_root
        .to_str()
        .context("catalog root is not UTF-8")?
        .to_owned();

    for spec in specs {
        if spec.driver.is_some() {
            continue;
        }
        if spec.delivery != Some(DeliveryTransport::AppServer) {
            continue;
        }
        let bus_id = spec.bus_id(this_host);
        let mut candidates = spec
            .tasks
            .iter_mut()
            .filter(|task| !task.derived && task.name == "agent");
        let task = candidates.next().with_context(|| {
            format!(
                "agent '{bus_id}' selects `deliver \"app-server\"` but has no canonical `agent` task"
            )
        })?;
        anyhow::ensure!(
            candidates.next().is_none(),
            "agent '{bus_id}' selects `deliver \"app-server\"` with more than one canonical `agent` task"
        );
        anyhow::ensure!(
            task.kind == TaskKind::Pty,
            "agent '{bus_id}' selects `deliver \"app-server\"` for a non-PTY canonical task"
        );
        let authored = task.argv.clone().with_context(|| {
            format!(
                "agent '{bus_id}' selects `deliver \"app-server\"`; its canonical task must use structured `argv`, not shell `command`"
            )
        })?;
        anyhow::ensure!(
            !authored.is_empty(),
            "agent '{bus_id}' selects `deliver \"app-server\"` with an empty canonical argv"
        );
        anyhow::ensure!(
            !authored
                .iter()
                .any(|arg| arg == "--remote" || arg.starts_with("--remote=")),
            "agent '{bus_id}' selects `deliver \"app-server\"` but its canonical argv already declares `--remote`"
        );
        let runtime_id = task
            .id
            .clone()
            .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
        let mut argv = vec![
            st2_executable.clone(),
            "--catalog".to_string(),
            catalog_root.clone(),
            "codex-app-server".to_string(),
            "--identity".to_string(),
            bus_id,
            "--runtime-id".to_string(),
            runtime_id,
            "--".to_string(),
        ];
        argv.extend(authored);
        task.command = None;
        task.argv = Some(argv);
    }
    Ok(())
}

/// ACTUAL state: one running/known task as st2 observes it (unioned across backends).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The pinned task id — the key that matches a session back to a declared task.
    pub pty_id: String,
    /// `true` while running; `false` once exited/vanished (a GC candidate).
    pub alive: bool,
    /// The process exit code once exited (`None` while running, or if killed/vanished with no code).
    /// Reconcile ignores this; it exists only for crash-vs-clean-exit detection (the crash-ding).
    pub exit_code: Option<i64>,
    /// PTY presentation observed in the same authoritative inventory snapshot. Exec sessions and
    /// older/partial observations leave this unknown so reconciliation repairs them fail-closed.
    pub presentation: Option<ObservedPtyPresentation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPtyPresentation {
    pub display_name: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// A concrete task st2 should spawn — everything a backend needs, resolved from the spec. Produced
/// only for tasks that carry an explicit `command` or `argv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTarget {
    /// `pty` (terminal) or `exec` (terminal-free) — selects the backend.
    pub kind: TaskKind,
    /// Resolved task id (the spec's `id`, or `<bus_id>.<name>` fallback).
    pub pty_id: String,
    /// The agent bus id this task belongs to (`<host>.<identity>`).
    pub bus_id: String,
    /// The task name (`agent`, `ding`, …).
    pub name: String,
    /// Generated from another task rather than authored as an independent sibling.
    pub derived: bool,
    /// How to launch the task: shell source or a direct program argument vector.
    pub launch: TaskLaunch,
    /// Declared working dir; `None` → default to `workspace`, else the spec dir (resolved at spawn).
    pub cwd: Option<String>,
    /// The agent's workspace — the cwd default when `cwd` is unset.
    pub workspace: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    /// GC pin (task-level `keep`, or the agent-level `keep`).
    pub keep: bool,
    /// Desired PTY-only presentation projected at spawn. Exec tasks carry `None`.
    pub presentation: Option<PtyPresentation>,
}

/// Exact, non-lifecycle metadata desired for one managed PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyPresentation {
    /// Exact stable PTY task ID. Automation must never resolve this as a display alias.
    pub pty_id: String,
    /// `Some(value)` updates primary-agent display metadata; `Some(None)` clears it. `None` preserves
    /// a secondary task's existing task-specific display convention.
    pub display_name: Option<Option<String>>,
    /// Complete st2-owned tag snapshot. `None` removes an optional owned key.
    pub tags: BTreeMap<String, Option<String>>,
}

pub const AGENT_PRESENTATION_SCHEMA_TAG: &str = "agent.presentation.schema";
pub const AGENT_ACTOR_PATH_TAG: &str = "agent.actor.path";
pub const AGENT_DESCRIPTION_TAG: &str = "agent.presentation.description";
/// Compatibility role owned by st2 only on the canonical agent PTY.
pub const COMPATIBILITY_ROLE_TAG: &str = "role";

/// Fail-closed admission errors for runner-owned task identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskIdentityAdmissionError {
    Conflict {
        bus_id: String,
        task: String,
        declared: String,
    },
}

impl fmt::Display for TaskIdentityAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict {
                bus_id,
                task,
                declared,
            } => write!(
                formatter,
                "agent '{bus_id}' task '{task}' declares conflicting ST_AGENT '{declared}'; expected runner-owned value '{bus_id}'"
            ),
        }
    }
}

impl std::error::Error for TaskIdentityAdmissionError {}

/// Reject local active tasks whose authored identity conflicts with the runner-derived bus ID.
pub fn validate_task_identities(
    specs: &[AgentSpec],
    this_host: &str,
) -> Result<(), TaskIdentityAdmissionError> {
    for spec in specs {
        if !spec.desired_state.is_running() || spec.resolved_host(this_host) != this_host {
            continue;
        }
        let bus_id = spec.bus_id(this_host);
        for task in &spec.tasks {
            if let Some(declared) = task.env.get("ST_AGENT")
                && declared != &bus_id
            {
                return Err(TaskIdentityAdmissionError::Conflict {
                    bus_id,
                    task: task.name.clone(),
                    declared: declared.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Project runner-owned identity and the supervisor source of truth into one launch target.
fn runner_task_env(
    spec: &AgentSpec,
    task: &crate::spec::Task,
    bus_id: &str,
) -> BTreeMap<String, String> {
    let mut env = task.env.clone();
    env.insert("ST_AGENT".to_owned(), bus_id.to_owned());
    if let Some(supervisor) = &spec.supervisor {
        env.insert("ST_SUPERVISOR".to_owned(), supervisor.clone());
    } else {
        env.remove("ST_SUPERVISOR");
    }
    env
}

fn pty_presentation(
    spec: &AgentSpec,
    task: &crate::spec::Task,
    pty_id: &str,
    bus_id: &str,
) -> Option<PtyPresentation> {
    if task.kind != TaskKind::Pty {
        return None;
    }
    let canonical_agent = task.name == "agent" && pty_id == bus_id;
    Some(PtyPresentation {
        pty_id: pty_id.to_owned(),
        display_name: (task.name == "agent").then(|| match spec.name.as_ref() {
            Some(name) if name == pty_id => None,
            _ => spec.name.clone(),
        }),
        tags: BTreeMap::from([
            (
                AGENT_PRESENTATION_SCHEMA_TAG.to_owned(),
                Some("1".to_owned()),
            ),
            (AGENT_ACTOR_PATH_TAG.to_owned(), Some(bus_id.to_owned())),
            (AGENT_DESCRIPTION_TAG.to_owned(), spec.description.clone()),
            (
                COMPATIBILITY_ROLE_TAG.to_owned(),
                canonical_agent.then(|| "agent".to_owned()),
            ),
        ]),
    })
}

fn presentation_matches(desired: &PtyPresentation, observed: &ObservedPtyPresentation) -> bool {
    let display_name_matches = desired
        .display_name
        .as_ref()
        .is_none_or(|display_name| display_name == &observed.display_name);
    display_name_matches
        && desired
            .tags
            .iter()
            .all(|(key, value)| observed.tags.get(key) == value.as_ref())
}

/// A resolved task launch accepted by the execution backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskLaunch {
    /// Shell source, preserved verbatim and passed to `sh -c`.
    Shell(String),
    /// A non-empty vector whose first element is the program.
    Argv(Vec<String>),
}

/// An agent to launch, with the specific tasks that are missing (not already live).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch<'a> {
    pub spec: &'a AgentSpec,
    pub tasks: Vec<TaskTarget>,
    /// Exact derived task IDs proved live in the same inventory snapshot. Execution stops these if
    /// the canonical agent becomes terminal while applying this launch.
    pub live_derived: Vec<String>,
}

/// Exact live task IDs to stop because their owner retired or their derived target is ineligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teardown<'a> {
    pub spec: &'a AgentSpec,
    pub pty_ids: Vec<String>,
}

/// The reconcile plan — DESIRED vs ACTUAL, host-filtered. Pure output; execution applies it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan<'a> {
    /// This host, active service, with ≥1 declared task not currently live → spawn the missing tasks.
    pub launch: Vec<Launch<'a>>,
    /// This host, retired, with live sessions → kill them.
    pub teardown: Vec<Teardown<'a>>,
    /// This host, retired → archive every inbox message, even when no session remains.
    pub settle_retirement: Vec<&'a AgentSpec>,
    /// This host, active service, every declared task already present (live, or dead+`keep` frozen).
    pub adopt: Vec<&'a AgentSpec>,
    /// host != this machine → skipped; another machine's st2 owns it.
    pub other_host: Vec<&'a AgentSpec>,
    /// This host, active service, but no task carries a launch (unrendered) → nothing to run.
    pub unrunnable: Vec<&'a AgentSpec>,
    /// Dead, non-`keep` sessions of declared tasks → reap (`rm`).
    pub gc: Vec<String>,
    /// Dead or absent `adopt-only` task ids held without reap or launch.
    pub held: Vec<String>,
    /// In-place presentation updates for healthy managed PTYs, independent of lifecycle actions.
    pub presentation: Vec<PtyPresentation>,
    /// Declared task ids this pass PROVED alive in the inventory snapshot — the positive evidence
    /// the restart cap needs. It is deliberately not "everything we did not launch": a task can be
    /// missing from `launch` because it was never considered (its owner failed to materialize, its
    /// launch was gated, its death was debounced as a flicker), and inferring liveness from that
    /// silence credits uptime to a task nobody looked at. Excludes ids headed for `teardown`.
    pub live: Vec<String>,
}

/// Resolve one exact local task selector (`host.agent.task` or explicit task id) without mutation.
pub fn resolve_task<'a>(
    specs: &'a [AgentSpec],
    selector: &str,
    this_host: &str,
) -> anyhow::Result<(&'a AgentSpec, &'a crate::spec::Task, String)> {
    let mut matches = Vec::new();
    for spec in specs {
        if spec.resolved_host(this_host) != this_host {
            continue;
        }
        for task in &spec.tasks {
            let runtime = task
                .id
                .clone()
                .unwrap_or_else(|| format!("{}.{}", spec.bus_id(this_host), task.name));
            let qualified = format!("{}.{}", spec.bus_id(this_host), task.name);
            if selector == runtime || selector == qualified {
                matches.push((spec, task, runtime));
            }
        }
    }
    match matches.as_slice() {
        [(spec, task, runtime)] => Ok((*spec, *task, runtime.clone())),
        [] => anyhow::bail!("task selector {selector:?} did not resolve to one local task"),
        _ => anyhow::bail!("task selector {selector:?} is ambiguous"),
    }
}

/// Pure task-scoped plan: resolve first, then retain only the selected runtime target.
pub fn reconcile_selected<'a>(
    specs: &'a [AgentSpec],
    sessions: &[Session],
    this_host: &str,
    selector: &str,
) -> anyhow::Result<ReconcilePlan<'a>> {
    validate_task_identities(specs, this_host)?;
    let (owner, task, runtime) = resolve_task(specs, selector, this_host)?;
    let mut plan = ReconcilePlan::default();
    if owner.desired_state.is_retired() {
        plan.settle_retirement.push(owner);
    }
    let actual = sessions.iter().find(|s| s.pty_id == runtime);
    if !owner.desired_state.is_running() {
        if let Some(s) = actual {
            if s.alive {
                plan.teardown.push(Teardown {
                    spec: owner,
                    pty_ids: vec![runtime],
                });
            } else if owner.desired_state.is_retired() || !(task.keep || owner.keep) {
                plan.gc.push(runtime);
            }
        }
        return Ok(plan);
    }
    if task.derived && !actual.is_some_and(|session| session.alive) {
        plan.held.push(runtime);
        return Ok(plan);
    }
    let launch = match (&task.command, &task.argv) {
        (Some(command), None) => TaskLaunch::Shell(command.clone()),
        (None, Some(argv)) => TaskLaunch::Argv(argv.clone()),
        (None, None) => {
            plan.unrunnable.push(owner);
            return Ok(plan);
        }
        (Some(_), Some(_)) => {
            unreachable!("discovery rejects tasks carrying both command and argv")
        }
    };
    let bus_id = owner.bus_id(this_host);
    let env = runner_task_env(owner, task, &bus_id);
    let target = TaskTarget {
        kind: task.kind,
        pty_id: runtime.clone(),
        bus_id: bus_id.clone(),
        name: task.name.clone(),
        derived: task.derived,
        launch,
        cwd: task.cwd.clone(),
        workspace: owner.workspace.clone(),
        tags: task.tags.clone(),
        env,
        keep: task.keep || owner.keep,
        presentation: pty_presentation(owner, task, &runtime, &bus_id),
    };
    match actual {
        Some(s) if s.alive => {
            if let Some(presentation) = target.presentation.clone()
                && !s
                    .presentation
                    .as_ref()
                    .is_some_and(|observed| presentation_matches(&presentation, observed))
            {
                plan.presentation.push(presentation);
            }
            plan.live.push(runtime);
            plan.adopt.push(owner);
        }
        _ if task.lifecycle == TaskLifecycle::AdoptOnly => plan.held.push(runtime),
        Some(_) if target.keep => plan.adopt.push(owner),
        Some(_) => {
            plan.gc.push(runtime);
            plan.launch.push(Launch {
                spec: owner,
                tasks: vec![target],
                live_derived: Vec::new(),
            });
        }
        _ => plan.launch.push(Launch {
            spec: owner,
            tasks: vec![target],
            live_derived: Vec::new(),
        }),
    }
    Ok(plan)
}

/// The state of a declared task's session in the ACTUAL world.
#[derive(Clone, Copy)]
enum SessionState {
    Alive,
    Dead,
    Absent,
}

fn session_state(by_id: &HashMap<&str, bool>, pty_id: &str) -> SessionState {
    match by_id.get(pty_id) {
        Some(true) => SessionState::Alive,
        Some(false) => SessionState::Dead,
        None => SessionState::Absent,
    }
}

/// Resolve a task's on-disk id: the explicit `id`, else `<bus_id>.<name>`. This is the session
/// name `pty` binds a socket for, so admission checks resolve it through here rather than
/// re-deriving the format.
pub(crate) fn resolve_task_id(bus_id: &str, name: &str, explicit: Option<&str>) -> String {
    match explicit {
        Some(id) => id.to_string(),
        None => format!("{bus_id}.{name}"),
    }
}

/// Compute the reconcile plan for `specs` given observed `sessions`, filtering to `this_host`.
pub fn reconcile<'a>(
    specs: &'a [AgentSpec],
    sessions: &[Session],
    this_host: &str,
) -> Result<ReconcilePlan<'a>, TaskIdentityAdmissionError> {
    validate_task_identities(specs, this_host)?;
    let by_id: HashMap<&str, bool> = sessions
        .iter()
        .map(|s| (s.pty_id.as_str(), s.alive))
        .collect();
    let sessions_by_id: HashMap<&str, &Session> = sessions
        .iter()
        .map(|session| (session.pty_id.as_str(), session))
        .collect();

    let mut plan = ReconcilePlan::default();
    for spec in specs {
        if spec.resolved_host(this_host) != this_host {
            plan.other_host.push(spec);
            continue;
        }
        let bus_id = spec.bus_id(this_host);

        if !spec.desired_state.is_running() {
            if spec.desired_state.is_retired() {
                plan.settle_retirement.push(spec);
            }
            let mut teardown_ids = Vec::new();
            for t in &spec.tasks {
                let id = resolve_task_id(&bus_id, &t.name, t.id.as_deref());
                let retain_dead = spec.desired_state.is_suspended() && (t.keep || spec.keep);
                match session_state(&by_id, &id) {
                    SessionState::Alive => teardown_ids.push(id),
                    SessionState::Dead if !retain_dead => plan.gc.push(id),
                    _ => {}
                }
            }
            if !teardown_ids.is_empty() {
                plan.teardown.push(Teardown {
                    spec,
                    pty_ids: teardown_ids,
                });
            }
            continue;
        }

        if !spec.is_runnable() {
            plan.unrunnable.push(spec);
            continue;
        }

        let targets: Vec<(TaskTarget, TaskLifecycle)> = spec
            .tasks
            .iter()
            .filter_map(|t| {
                let launch = match (&t.command, &t.argv) {
                    (Some(command), None) => TaskLaunch::Shell(command.clone()),
                    (None, Some(argv)) => TaskLaunch::Argv(argv.clone()),
                    (None, None) => return None,
                    (Some(_), Some(_)) => {
                        unreachable!("discovery rejects tasks carrying both command and argv")
                    }
                };
                let env = runner_task_env(spec, t, &bus_id);
                let pty_id = resolve_task_id(&bus_id, &t.name, t.id.as_deref());
                Some((
                    TaskTarget {
                        kind: t.kind,
                        pty_id: pty_id.clone(),
                        bus_id: bus_id.clone(),
                        name: t.name.clone(),
                        derived: t.derived,
                        launch,
                        cwd: t.cwd.clone(),
                        workspace: spec.workspace.clone(),
                        tags: t.tags.clone(),
                        env,
                        keep: t.keep || spec.keep,
                        presentation: pty_presentation(spec, t, &pty_id, &bus_id),
                    },
                    t.lifecycle,
                ))
            })
            .collect();

        debug_assert!(!targets.is_empty());

        let agent_eligible = targets
            .iter()
            .find(|(target, _)| target.name == "agent" && !target.derived)
            .is_some_and(
                |(target, lifecycle)| match session_state(&by_id, &target.pty_id) {
                    SessionState::Alive => true,
                    SessionState::Dead => !target.keep && *lifecycle == TaskLifecycle::Service,
                    SessionState::Absent => *lifecycle == TaskLifecycle::Service,
                },
            );
        let mut to_launch = Vec::new();
        let mut live_derived = Vec::new();
        let mut ineligible_derived = Vec::new();
        let mut derived_cleanup = false;
        let held_before = plan.held.len();
        for (target, lifecycle) in targets {
            let state = session_state(&by_id, &target.pty_id);
            if target.derived && !agent_eligible {
                match state {
                    SessionState::Alive => {
                        ineligible_derived.push(target.pty_id);
                        derived_cleanup = true;
                    }
                    SessionState::Dead if !target.keep => {
                        plan.gc.push(target.pty_id);
                        derived_cleanup = true;
                    }
                    SessionState::Dead | SessionState::Absent => {}
                }
                continue;
            }
            match state {
                SessionState::Alive => {
                    plan.live.push(target.pty_id.clone());
                    if target.derived {
                        live_derived.push(target.pty_id.clone());
                    }
                    let actual = sessions_by_id
                        .get(target.pty_id.as_str())
                        .expect("alive state has a session");
                    if let Some(presentation) = target.presentation.clone()
                        && !actual
                            .presentation
                            .as_ref()
                            .is_some_and(|observed| presentation_matches(&presentation, observed))
                    {
                        plan.presentation.push(presentation);
                    }
                }
                SessionState::Dead | SessionState::Absent
                    if lifecycle == TaskLifecycle::AdoptOnly =>
                {
                    plan.held.push(target.pty_id.clone());
                }
                SessionState::Dead if target.keep => {}
                SessionState::Dead => {
                    plan.gc.push(target.pty_id.clone());
                    to_launch.push(target);
                }
                SessionState::Absent => to_launch.push(target),
            }
        }
        if !ineligible_derived.is_empty() {
            plan.teardown.push(Teardown {
                spec,
                pty_ids: ineligible_derived,
            });
        }

        if to_launch.is_empty() && plan.held.len() == held_before && !derived_cleanup {
            plan.adopt.push(spec);
        } else if !to_launch.is_empty() {
            plan.launch.push(Launch {
                spec,
                tasks: to_launch,
                live_derived,
            });
        }
    }
    Ok(plan)
}
