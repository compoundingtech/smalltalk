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

use agent_spec::spec::{AgentSpec, TaskKind, TaskLifecycle};

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
/// Stable run role owned by st2 only on the canonical agent PTY.
pub const RUN_ROLE_TAG: &str = "run.role";

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
        if spec.retired || spec.resolved_host(this_host) != this_host {
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
            (
                RUN_ROLE_TAG.to_owned(),
                canonical_agent.then(|| "coding-agent".to_owned()),
            ),
        ]),
    })
}

fn presentation_matches(
    desired: &PtyPresentation,
    observed: &ObservedPtyPresentation,
) -> bool {
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
    let actual = sessions.iter().find(|s| s.pty_id == runtime);
    if owner.retired {
        if let Some(s) = actual {
            if s.alive {
                plan.teardown.push(Teardown {
                    spec: owner,
                    pty_ids: vec![runtime],
                });
            } else if !(task.keep || owner.keep) {
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

/// Resolve a task's on-disk id: the explicit `id`, else `<bus_id>.<name>`.
fn resolve_task_id(bus_id: &str, name: &str, explicit: Option<&str>) -> String {
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

        if spec.retired {
            let mut teardown_ids = Vec::new();
            for t in &spec.tasks {
                let id = resolve_task_id(&bus_id, &t.name, t.id.as_deref());
                let keep = t.keep || spec.keep;
                match session_state(&by_id, &id) {
                    SessionState::Alive => teardown_ids.push(id),
                    SessionState::Dead if !keep => plan.gc.push(id),
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
            .is_some_and(|(target, lifecycle)| match session_state(&by_id, &target.pty_id) {
                SessionState::Alive => true,
                SessionState::Dead => {
                    !target.keep && *lifecycle == TaskLifecycle::Service
                }
                SessionState::Absent => *lifecycle == TaskLifecycle::Service,
            });
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
