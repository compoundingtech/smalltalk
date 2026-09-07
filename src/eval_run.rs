//! Runtime for the st2 spec (P2+): boot a spec's team, and (P3/P4) run its `eval` + judges. The
//! team-boot REUSES the existing reconcile/execute machinery. Compact `team` declarations map to
//! in-memory [`AgentSpec`]s; an explicit `canonical-agents` eval instead discovers the post-run
//! hermetic catalog. Either way one [`AgentSpec`] vector flows through reconcile, execution,
//! supervision, and teardown exactly as `st2 up <catalog>` does.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::eval_spec::{
    Check, Eval, Judge, JudgeKind, RunStep, Spec, SpecAgent, ding_exec, parse_spec,
};
use crate::expand::expand_catalog;
use crate::flapping::FlappingCap;
#[cfg(test)]
use crate::reconcile::compile_generated_ding_tasks;
use crate::reconcile::{TaskCompileContext, compile_generated_tasks, reconcile};
use crate::run::{Runner, SystemRunner, UpReport, detect_host, execute};
use agent_spec::spec::{AgentDesiredState, AgentSpec, JobType, Task, TaskKind, TaskLifecycle};

macro_rules! eval_log {
    ($($arg:tt)*) => {
        if std::env::var_os("ST2_EVAL_JSON").is_some() { eprintln!($($arg)*); } else { std::println!($($arg)*); }
    };
}

static EVAL_INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_eval_signal(_signal: libc::c_int) {
    EVAL_INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_eval_signal_handlers() -> (libc::sighandler_t, libc::sighandler_t) {
    EVAL_INTERRUPTED.store(false, Ordering::SeqCst);
    // libc exposes sighandler_t as a numeric ABI token on some targets.
    #[allow(clippy::fn_to_numeric_cast, function_casts_as_integer)]
    unsafe {
        (
            libc::signal(libc::SIGINT, on_eval_signal as libc::sighandler_t),
            libc::signal(libc::SIGTERM, on_eval_signal as libc::sighandler_t),
        )
    }
}

fn restore_eval_signal_handlers(previous: (libc::sighandler_t, libc::sighandler_t)) {
    unsafe {
        libc::signal(libc::SIGINT, previous.0);
        libc::signal(libc::SIGTERM, previous.1);
    }
}

struct EvalSignalGuard((libc::sighandler_t, libc::sighandler_t));
impl Drop for EvalSignalGuard {
    fn drop(&mut self) {
        restore_eval_signal_handlers(self.0);
    }
}

/// Map a parsed spec's agents into in-memory [`AgentSpec`]s rooted at `root` (which becomes `$CATALOG`
/// and the base for each agent's `workspace`/cwd). Each agent's own `command` is a `pty` task keyed by
/// the agent id; each `exec` block is an `exec` task keyed by its id. Env is already cascaded.
pub fn spec_to_agent_specs(agents: &[SpecAgent], host: &str, root: &Path) -> Vec<AgentSpec> {
    // `spec.path.parent()` is the cwd/`$CATALOG` base in reconcile/execute → point it at `root`.
    let path = root.join("spec.kdl");
    agents
        .iter()
        .map(|a| {
            let host_prefix = format!("{host}.");
            let identity = a.id.strip_prefix(&host_prefix).unwrap_or(&a.id).to_owned();
            let actor_id = format!("{host}.{identity}");
            let runtime_id = |source: &str| match source.strip_prefix(&a.id) {
                Some("") => actor_id.clone(),
                Some(suffix) if suffix.starts_with('.') => format!("{actor_id}{suffix}"),
                _ => source.to_owned(),
            };
            let mut tasks = Vec::new();
            tasks.push(Task {
                kind: TaskKind::Pty,
                derived: false,
                name: "agent".to_string(),
                id: Some(actor_id.clone()),
                command: Some(a.command.clone()),
                argv: None,
                cwd: None, // → the agent's workspace (resolved relative to `root`)
                tags: BTreeMap::new(),
                env: a.env.clone(),
                keep: false,
                lifecycle: TaskLifecycle::Service,
            });
            for ex in &a.execs {
                let command = if ex.derived {
                    ding_exec(&actor_id).command
                } else {
                    ex.command.clone()
                };
                tasks.push(Task {
                    kind: TaskKind::Exec,
                    derived: ex.derived,
                    name: ex.id.clone(),
                    id: Some(runtime_id(&ex.id)),
                    command: Some(command),
                    argv: None,
                    cwd: None,
                    tags: BTreeMap::new(),
                    env: ex.env.clone(),
                    keep: false,
                    lifecycle: TaskLifecycle::Service,
                });
            }
            AgentSpec {
                id: None,
                address: None,
                identity,
                name: a.name.clone(),
                description: a.description.clone(),
                host: Some(host.to_string()),
                role: None,
                job_type: JobType::Service,
                workspace: a.workspace.clone(),
                supervisor: a.supervisor.as_deref().map(|supervisor| {
                    supervisor
                        .strip_prefix(&host_prefix)
                        .map_or_else(|| format!("{host}.{supervisor}"), |_| supervisor.to_owned())
                }),
                desired_state: AgentDesiredState::Running,
                keep: false,
                restart: None,
                delivery: None,
                session_driver: None,
                driver: None,
                delivery_readiness: None,
                resources: Vec::new(),
                streams: Vec::new(),
                tasks,
                path: path.clone(),
            }
        })
        .collect()
}

#[derive(Debug)]
struct CanonicalEvalTeam {
    specs: Vec<AgentSpec>,
    routes: BTreeMap<String, CanonicalRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvalRuntimeTask {
    agent_id: String,
    runtime_id: String,
    is_pty: bool,
}

#[derive(Debug, Clone)]
struct CanonicalRoute {
    inbox: PathBuf,
    archive: PathBuf,
}

fn admitted_route<'a>(
    routes: &'a BTreeMap<String, CanonicalRoute>,
    id: &str,
) -> &'a CanonicalRoute {
    routes
        .get(id)
        .unwrap_or_else(|| panic!("strict canonical admission did not freeze route for `{id}`"))
}

fn task_runtime_id(spec: &AgentSpec, task: &Task, host: &str) -> String {
    task.id
        .clone()
        .unwrap_or_else(|| format!("{}.{}", spec.bus_id(host), task.name))
}

fn task_is_launchable(task: &Task) -> bool {
    task.command.is_some() || task.argv.is_some()
}

/// Project runtime inventory only after generated launch tasks are compiled.
fn eval_runtime_tasks(specs: &[AgentSpec], host: &str) -> Vec<EvalRuntimeTask> {
    let mut tasks = specs
        .iter()
        .flat_map(|spec| {
            spec.tasks
                .iter()
                .filter(|task| task_is_launchable(task))
                .map(|task| EvalRuntimeTask {
                    agent_id: spec.bus_id(host),
                    runtime_id: task_runtime_id(spec, task, host),
                    is_pty: task.kind == TaskKind::Pty,
                })
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| left.runtime_id.cmp(&right.runtime_id));
    tasks
}

/// Discover the sole declaration authority for a `canonical-agents` eval after its fixture and run
/// steps have populated the hermetic catalog. This deliberately consumes the shared Agent Spec
/// parser instead of projecting the compact eval grammar into a second, partial declaration.
fn load_canonical_eval_team(catalog: &Path, host: &str) -> Result<CanonicalEvalTeam> {
    let validation = crate::validate::validate_for_host(catalog, host);
    if !validation.issues.is_empty() {
        let issues = validation
            .issues
            .iter()
            .map(|issue| {
                format!(
                    "{} {} {}: {}",
                    issue.severity.tag(),
                    issue.code,
                    issue.path,
                    issue.message
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("canonical eval Agent Specs failed strict validation: {issues}");
    }
    let found = crate::discover(catalog);
    if !found.errors.is_empty() {
        let errors = found
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("canonical eval Agent Spec discovery failed: {errors}");
    }
    if !found.warnings.is_empty() {
        anyhow::bail!(
            "canonical eval Agent Specs must discover without warnings: {}",
            found.warnings.join("; ")
        );
    }
    if crate::catalog::pty_root(catalog) != catalog.join("pty") {
        anyhow::bail!(
            "canonical-agents requires the hermetic PTY root `{}`",
            catalog.join("pty").display()
        );
    }

    let local_specs = found
        .specs
        .iter()
        .filter(|spec| spec.resolved_host(host) == host)
        .cloned()
        .collect::<Vec<_>>();
    if local_specs.is_empty() {
        anyhow::bail!(
            "canonical-agents found no local canonical Agent Specs for host `{host}` in {}",
            catalog.display()
        );
    }

    let mut bus_ids = HashSet::new();
    let mut runtime_ids = BTreeMap::<String, String>::new();
    let mut routes = BTreeMap::new();
    for spec in &local_specs {
        let bus_id = spec.bus_id(host);
        if !bus_ids.insert(bus_id.clone()) {
            anyhow::bail!("canonical-agents found duplicate Agent Spec bus identity `{bus_id}`");
        }
        if !spec.desired_state.is_running() {
            anyhow::bail!(
                "canonical-agents refuses non-running Agent Spec `{bus_id}` ({})",
                spec.desired_state.as_str()
            );
        }
        for task in &spec.tasks {
            for root in ["CATALOG", "ST_ROOT", "PTY_ROOT", "ST2_EVAL_REQUESTER"] {
                if task.env.contains_key(root) {
                    anyhow::bail!(
                        "canonical-agents Agent Spec `{bus_id}` must not override eval-owned `{root}`"
                    );
                }
            }
        }
        for task in &spec.tasks {
            let runtime_id = task_runtime_id(spec, task, host);
            if runtime_id.trim().is_empty() {
                anyhow::bail!(
                    "canonical-agents Agent Spec `{bus_id}` task `{}` runtime task id must be nonempty",
                    task.name
                );
            }
            if let Some(previous) = runtime_ids.insert(runtime_id.clone(), bus_id.clone()) {
                anyhow::bail!(
                    "canonical-agents found duplicate runtime task id `{runtime_id}` in `{previous}` and `{bus_id}`"
                );
            }
        }
        let agent_dir = spec
            .path
            .parent()
            .expect("canonical Agent Spec path has an agent directory");
        let route = CanonicalRoute {
            inbox: crate::message::inbox_dir(agent_dir),
            archive: crate::message::archive_dir(agent_dir),
        };
        for spelling in [bus_id, spec.identity.clone()] {
            match routes.entry(spelling.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(route.clone());
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    anyhow::bail!(
                        "canonical-agents found duplicate canonical route spelling `{spelling}`"
                    );
                }
            }
        }
    }
    let materialized = crate::materialize::materialize_catalog(catalog, &local_specs, host);
    if !materialized.errors.is_empty() {
        anyhow::bail!(
            "canonical eval Agent Spec materialization failed: {}",
            materialized.errors.join("; ")
        );
    }
    if !materialized.warnings.is_empty() {
        anyhow::bail!(
            "canonical eval Agent Spec materialization warnings are fatal: {}",
            materialized.warnings.join("; ")
        );
    }
    Ok(CanonicalEvalTeam {
        specs: local_specs,
        routes,
    })
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Wrap supervised eval seats so their natural exit code survives PTY metadata sweeping. A killed
/// wrapper cannot write the marker, which deliberately remains distinguishable from a clean exit.
fn add_eval_exit_markers(specs: &mut [AgentSpec], catalog: &Path, host: &str) {
    let marker_dir = catalog.join(".eval-exits");
    let _ = std::fs::create_dir_all(&marker_dir);
    for spec in specs {
        let Some(index) = spec
            .tasks
            .iter()
            .position(|task| task.kind == TaskKind::Pty)
        else {
            continue;
        };
        let runtime_id = task_runtime_id(spec, &spec.tasks[index], host);
        let task = &mut spec.tasks[index];
        let Some(command) = task.command.take() else {
            continue;
        };
        let marker = marker_dir.join(format!("{runtime_id}.status"));
        let script = concat!(
            "marker=$1; command=$2; rm -f \"$marker\"; ",
            "sh -c \"$command\"; code=$?; ",
            "tmp=\"$marker.$$\"; printf '%s\\n' \"$code\" > \"$tmp\"; ",
            "mv \"$tmp\" \"$marker\"; exit \"$code\""
        );
        task.command = Some(format!(
            "sh -c {} st2-exit-marker {} {}",
            shell_single_quote(script),
            shell_single_quote(&marker.display().to_string()),
            shell_single_quote(&command)
        ));
    }
}

fn eval_exit_code(catalog: &Path, identity: &str) -> Option<i64> {
    std::fs::read_to_string(
        catalog
            .join(".eval-exits")
            .join(format!("{identity}.status")),
    )
    .ok()?
    .trim()
    .parse()
    .ok()
}

/// Strip the LAUNCHER's agent-identity env so a spawned harness seat runs as a FRESH top-level agent,
/// not a nested child. When `st2 eval`/`st2 up` is itself launched from INSIDE a claude/codex session
/// (e.g. evals-claude running `st2 eval`), those session-identity vars (`CLAUDECODE`,
/// `CLAUDE_CODE_SESSION_ID`, `CLAUDE_PID`, …) leak into the seats and make a nested claude behave as a
/// child/one-shot — it exits after the boot turn instead of staying interactive (the seat-persistence
/// failure). A fresh top-level harness has none of these, so the child must not inherit them.
/// `ANTHROPIC_*` (API creds) is deliberately kept — only the per-session identity is stripped.
///
/// pi is the same shape: it exports `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_CODING_AGENT`, and the
/// selected model into every child, and st2's own `ST2_PI_CHANNEL_*` would hand a nested seat its
/// launcher's bus identity and inbox. Provider credentials are not `PI_`-prefixed, so stripping the
/// prefix wholesale costs nothing.
fn sanitize_agent_env() {
    let should_strip = |k: &str| {
        matches!(
            k,
            "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT" | "AI_AGENT"
        ) || k.starts_with("CLAUDE_CODE_")
            || k.starts_with("CODEX_")
            || k.starts_with("PI_")
            || k.starts_with("ST2_PI_CHANNEL_")
    };
    let victims: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| should_strip(k))
        .collect();
    // SAFETY: called before spawning seats, single-threaded (same contract as the PTY_ROOT/PATH sets).
    for k in victims {
        unsafe { std::env::remove_var(&k) };
    }
}

/// One boot pass: reconcile the in-memory specs against live sessions and spawn what's missing
/// (adopting anything already alive). `root` roots `$CATALOG`; sessions land in the effective PTY_ROOT.
pub fn boot_team(agent_specs: &[AgentSpec], host: &str, root: &Path) -> Result<UpReport> {
    sanitize_agent_env(); // seats must boot as fresh top-level agents, not nested children of the launcher
    // HERMETIC exec state: `<catalog>/exec`, NOT the shared per-host `exec_state_dir(host)`. The eval's
    // PTY_ROOT is already hermetic, but exec-task state (the dings) lived in a per-HOST dir — so an
    // eval's `list_sessions()` unioned OTHER concurrent evals' + the LIVE FLEET's exec tasks, and its
    // supervise/teardown sweep could reap them (cross-eval corruption + fleet ding-flapping). Rooting it
    // under the catalog makes every eval fully isolated — it can only ever see/reap its OWN sessions.
    let runner = SystemRunner::new(root.to_path_buf(), root.join("exec"));
    boot_team_with_runner(agent_specs, host, &runner)
}

fn boot_team_with_runner(
    agent_specs: &[AgentSpec],
    host: &str,
    runner: &dyn Runner,
) -> Result<UpReport> {
    crate::reconcile::validate_task_identities(agent_specs, host)?;
    let sessions = runner.list_sessions().context("listing pty sessions")?;
    let plan = reconcile(agent_specs, &sessions, host)?;
    let mut report = UpReport::default();
    let mut cap = FlappingCap::default();
    execute(&plan, runner, &mut cap, &mut report);
    Ok(report)
}

fn supervised_eval_sessions(
    agent_specs: &[AgentSpec],
    host: &str,
    runner: &dyn Runner,
) -> Result<Vec<crate::reconcile::Session>> {
    crate::reconcile::validate_task_identities(agent_specs, host)?;
    runner
        .list_sessions()
        .context("listing supervised eval sessions")
}

/// Resolve a spec argument to `(spec-file, root-dir)`: a `*.kdl` FILE → that file (root = its dir); a
/// DIR with exactly one top-level `*.kdl` that parses as a spec → that file (root = the dir). Returns
/// `None` if it's not a spec (so `st2 up` falls back to catalog discovery).
pub fn resolve_spec_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return (path.extension().is_some_and(|x| x == "kdl")).then(|| path.to_path_buf());
    }
    if path.is_dir() {
        let kdls: Vec<PathBuf> = std::fs::read_dir(path)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "kdl"))
            .collect();
        if let [one] = kdls.as_slice() {
            // Confirm it parses as a spec (vs a stray .kdl in a catalog dir).
            if std::fs::read_to_string(one)
                .ok()
                .and_then(|t| parse_spec(&t).ok())
                .is_some()
            {
                return Some(one.clone());
            }
        }
    }
    None
}

/// Load + parse a spec file, returning `(Spec, root)` where `root` is the spec's folder (`$CATALOG`
/// and the base for `workspace`/cwd/`copy`/`content` resolution).
pub fn load_spec(spec_file: &Path) -> Result<(Spec, PathBuf)> {
    let text = std::fs::read_to_string(spec_file)
        .with_context(|| format!("reading spec {}", spec_file.display()))?;
    let spec = parse_spec(&text)?;
    let root = spec_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = root.canonicalize().unwrap_or(root);
    Ok((spec, root))
}

/// Prepare the launcher's env before spawning harness seats: (1) strip the launcher's agent-identity
/// vars so each seat boots as a fresh top-level agent, and (2) prepend this binary's dir to PATH for
/// authored bare `st2` commands. Generated DING argv does not consult PATH. Idempotent and called
/// before any seat spawns.
pub fn prepare_spawn_env(st2_executable: &Path) {
    sanitize_agent_env();
    if let Some(dir) = st2_executable.parent() {
        let path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", format!("{}:{path}", dir.display())) };
    }
}

// ── P3: the `st2 eval` flow ───────────────────────────────────────────────────────────────────────

/// The outcome of an eval run: whether the team reached "done", plus every judge's result. The
/// verdict is all-must-pass over the judges. Compact-team `done` remains informational; a canonical
/// team adds its completion result as a gating judge while still grading the final state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvalReport {
    /// The team reached the done signal (a sup→requester confirmation post-dating a worker report).
    pub done: bool,
    /// Every judge, in declared order (no short-circuit — a full legible report).
    pub judges: Vec<JudgeResult>,
    /// The whole-eval timeout that bounded the wait.
    pub timeout: Duration,
}

impl EvalReport {
    /// PASS iff there is at least one judge and every judge passed (all-must-pass).
    pub fn passed(&self) -> bool {
        // A pass requires at least one GATING (non-signal) judge, and every gating judge passing.
        // Signal judges run + show but never gate; an eval with only signal judges asserts nothing.
        let mut gating = self.judges.iter().filter(|j| !j.signal).peekable();
        gating.peek().is_some() && gating.all(|j| j.passed)
    }
}

/// One judge's outcome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JudgeResult {
    pub name: String,
    pub passed: bool,
    /// A short human-readable reason (what passed/failed) — for the legible report.
    pub detail: String,
    /// A SIGNAL result (from a `signal` judge): shown in the report but NOT counted toward the verdict.
    pub signal: bool,
}

/// Recursively copy `src`'s CONTENTS into `dst`, renaming any directory named `_git` → `.git`. A
/// committed fixture ships its repo db as `_git` (a real `.git` would nest as a gitlink); this
/// reconstructs a working repo in the run catalog with the working tree left as readable files.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let name = entry.file_name();
        let dst_name = if name == "_git" {
            std::ffi::OsString::from(".git")
        } else {
            name
        };
        let to = dst.join(&dst_name);
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// The bus root — the spec's top-level `ST_ROOT` with `$CATALOG` expanded, else the native flat
/// catalog root. Task runners use that same flat root when no task-level `ST_ROOT` is authored.
fn bus_root(spec: &Spec, catalog: &Path) -> PathBuf {
    match spec.env.get("ST_ROOT") {
        Some(v) => PathBuf::from(expand_catalog(v, catalog)),
        None => catalog.to_path_buf(),
    }
}

/// The kickoff content — a file (relative to the spec folder) if it exists, else inline text.
fn resolve_content(content: &str, spec_dir: &Path) -> Result<String> {
    let candidate = spec_dir.join(content);
    if candidate.is_file() {
        std::fs::read_to_string(&candidate)
            .with_context(|| format!("reading kickoff content {}", candidate.display()))
    } else {
        Ok(content.to_string())
    }
}

/// Whether a message `from:` header is `id` (tolerant of a `<prefix>.id` form, though the st2 spec
/// uses bare team-dotted ids).
fn from_is(from: Option<&str>, id: &str) -> bool {
    from.is_some_and(|f| f == id || f.ends_with(&format!(".{id}")))
}

/// Wait for the DONE signal, message-driven (not grade-poll). Multi-agent teams require a
/// `sup → requester` confirmation whose timestamp follows a `worker → sup` report. A canonical
/// singleton instead requires a causally new requester-inbox entry at-or-after the exact kickoff
/// receipt. Compact singleton semantics remain unchanged. Bounded by `timeout`. Returns whether done
/// fired.
fn wait_done(
    bus: &Path,
    canonical_routes: Option<&BTreeMap<String, CanonicalRoute>>,
    sup: &str,
    requester: &str,
    workers: &[String],
    kickoff_ts: Option<u64>,
    requester_before_kickoff: Option<&HashSet<String>>,
    timeout: Duration,
    on_tick: &mut dyn FnMut(),
) -> bool {
    let (sup_inbox, sup_archive) = match canonical_routes {
        Some(routes) => {
            let route = admitted_route(routes, sup);
            (route.inbox.clone(), route.archive.clone())
        }
        None => (bus.join(sup).join("inbox"), bus.join(sup).join("archive")),
    };
    // The requester is eval-owned, not an admitted Agent Spec, and deliberately keeps one explicit
    // flat mailbox. Every canonical agent route above comes from the frozen admitted vector.
    let req_inbox = bus.join(requester).join("inbox");
    let deadline = Instant::now() + timeout;
    loop {
        if EVAL_INTERRUPTED.load(Ordering::SeqCst) {
            return false;
        }
        // Earliest worker→sup report (a message from a worker agent). Scan inbox AND archive:
        // DING-BUS mandates "archive a message the moment you act on it", so a well-behaved sup MOVES the
        // report inbox→archive the instant it acts. Scanning inbox-only makes the done-signal a race
        // against the sup's archiving — a fully-closed loop hangs to max-timeout because the report left
        // the inbox. (The confirmation side reads the requester's inbox, which is safe — the requester is
        // a passive eval-runner seed that never archives.)
        let sup_msgs = crate::message::list_dir(&sup_inbox).unwrap_or_default();
        let sup_archived = crate::message::list_dir(&sup_archive).unwrap_or_default();
        if workers.is_empty()
            && let Some(kickoff_ts) = kickoff_ts
            && let Some(before) = requester_before_kickoff
        {
            let confirmed = crate::message::list_dir(&req_inbox)
                .unwrap_or_default()
                .iter()
                .any(|m| {
                    !before.contains(&m.filename)
                        && from_is(m.from.as_deref(), sup)
                        && m.ts_ms >= kickoff_ts
                });
            if confirmed {
                return true;
            }
        }
        let report_ts = sup_msgs
            .iter()
            .chain(sup_archived.iter())
            .filter(|m| workers.iter().any(|w| from_is(m.from.as_deref(), w)))
            .map(|m| m.ts_ms)
            .min();
        if let Some(rt) = report_ts {
            // A sup→requester confirmation at-or-after that report = the real done (not a bare ack).
            let confirmed = crate::message::list_dir(&req_inbox)
                .unwrap_or_default()
                .iter()
                .any(|m| from_is(m.from.as_deref(), sup) && m.ts_ms >= rt);
            if confirmed {
                return true;
            }
        }
        if Instant::now() > deadline {
            return false;
        }
        // A per-tick hook: under `supervise`, this respawns any dead team task FROM SPEC (full env) so
        // a fault-injected restart/crash recovers mid-run. A no-op for boot-once (unsupervised).
        on_tick();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Fail-fast boot gate: a task whose command exits immediately (127 harness-not-on-PATH, or a crash at
/// startup) must fail the eval LOUDLY now, not leave it hanging until `max-timeout` waiting for a
/// confirmation that can never come. Poll briefly for all tasks to be live — a real task is up within
/// ~1s; a dead-at-boot one never is (tolerant of a slow start + a transient pty-list flicker).
fn boot_gate(task_ids: &[String], specs: &[AgentSpec], host: &str, catalog: &Path) -> Result<()> {
    let runner = SystemRunner::new(catalog.to_path_buf(), catalog.join("exec"));
    let want: Vec<&str> = task_ids.iter().map(String::as_str).collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let sessions = runner.list_sessions().unwrap_or_default();
        let alive: HashSet<&str> = sessions
            .iter()
            .filter(|s| s.alive)
            .map(|s| s.pty_id.as_str())
            .collect();
        let dead: Vec<&str> = want
            .iter()
            .copied()
            .filter(|id| !alive.contains(id))
            .collect();
        if dead.is_empty() {
            return Ok(());
        }
        if Instant::now() > deadline {
            teardown_team(specs, host, catalog, false); // boot failure → no runtime tasks yet
            anyhow::bail!(
                "task(s) {dead:?} exited at boot — the command didn't stay running (harness not on PATH, \
                 e.g. claude/codex not installed, or a crash at startup). Failing fast instead of hanging \
                 until the eval's max-timeout."
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn require_canonical_boot(report: &UpReport, task_ids: &[String]) -> Result<()> {
    let missing = task_ids
        .iter()
        .filter(|id| !report.launched.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    if report.skipped
        || !report.errors.is_empty()
        || !report.flapping.is_empty()
        || !report.held.is_empty()
        || !report.unrunnable.is_empty()
        || !missing.is_empty()
    {
        anyhow::bail!(
            "canonical Agent Spec boot did not launch every admitted task: missing={missing:?}; \
             skipped={}; held={:?}; unrunnable={:?}; flapping={:?}; errors={:?}",
            report.skipped,
            report.held,
            report.unrunnable,
            report.flapping,
            report.errors
        );
    }
    Ok(())
}

fn message_timestamp(filename: &str) -> Result<u64> {
    filename
        .split_once('-')
        .and_then(|(timestamp, _)| timestamp.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("message receipt `{filename}` has no timestamp"))
}

/// Tear down the team (nomad-safe): mark the specs retired and reconcile → the runner kills the live
/// sessions (process-group kill). Best-effort — an eval always tears down, with no zombie tasks.
///
/// `reap_all` (set under `supervise`): after the declared teardown, ALSO reap every remaining session
/// in the eval's hermetic PTY_ROOT — runtime-spawned tasks that are NOT in the spec (e.g. a
/// team-standup specialist the CoS spun up mid-run). Declared teardown only reaps declared tasks, so
/// a runtime task would leak as an orphan; since the PTY_ROOT is hermetic to this eval, anything still
/// alive is ours to clean. Killing an already-dead declared session is a harmless no-op.
fn teardown_team_with_runner(specs: &[AgentSpec], host: &str, runner: &dyn Runner, reap_all: bool) {
    let retired: Vec<AgentSpec> = specs
        .iter()
        .cloned()
        .map(|mut s| {
            s.desired_state = AgentDesiredState::Retired { reason: None };
            s
        })
        .collect();
    if let Ok(sessions) = runner.list_sessions() {
        let plan = reconcile(&retired, &sessions, host)
            .expect("retired specs are excluded from task identity admission");
        let mut report = UpReport::default();
        let mut cap = FlappingCap::default();
        execute(&plan, runner, &mut cap, &mut report);
    }
    if reap_all && let Ok(remaining) = runner.list_sessions() {
        for s in &remaining {
            let _ = runner.kill(&s.pty_id);
            let _ = runner.remove(&s.pty_id);
        }
    }
}

fn teardown_team(specs: &[AgentSpec], host: &str, root: &Path, reap_all: bool) {
    let runner = SystemRunner::new(root.to_path_buf(), root.join("exec"));
    teardown_team_with_runner(specs, host, &runner, reap_all);
}

/// `st2 eval <folder>` — run the eval end to end: mint a hermetic temp catalog, copy the fixture
/// (`_git`→`.git`), boot the base team + eval-only agents, pretrust their workspaces, deliver the
/// kickoff, wait for the sup's confirmation (post-dating a worker report) or `max-timeout`, tear down.
/// (P4 runs the judges after done and returns the verdict.) The temp catalog is removed on the way out.
pub fn run_eval(spec_file: &Path, host: Option<String>, keep: bool) -> Result<EvalReport> {
    let _signal_guard = EvalSignalGuard(install_eval_signal_handlers());
    let (spec, spec_dir) = load_spec(spec_file)?;
    let eval = spec.eval.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no `eval {{}}` block — use `st2 up` to just boot the team",
            spec_file.display()
        )
    })?;
    // --host (explicit) › the spec's top-level `host` › the OS hostname.
    let host = host
        .or_else(|| spec.host.clone())
        .unwrap_or_else(detect_host);

    // Hermetic temp catalog + PTY_ROOT. Setting PTY_ROOT OVERRIDES any inherited ambient, so eval
    // sessions can NEVER leak into a live/prod pty registry (isolation by construction).
    let catalog = std::env::temp_dir().join(format!("st2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&catalog);
    std::fs::create_dir_all(&catalog)?;
    let _guard = EvalCleanupGuard {
        runner: SystemRunner::new(catalog.clone(), catalog.join("exec")),
        catalog: catalog.clone(),
        host: host.clone(),
        keep,
    };
    // SAFETY: st2 eval is single-threaded up to the boot; set before any seat spawns.
    unsafe { std::env::set_var("PTY_ROOT", catalog.join("pty")) };
    // Root exec-task state under the catalog too, so ANY st2 sub-invocation spawned INSIDE the eval — a
    // run-step's `st2 up`, a seat's bare `st2` — resolves `exec_state_dir(host)` = $XDG_STATE_HOME/st2/…
    // to an EVAL-LOCAL dir, not the shared per-host (live-fleet) one. The in-process eval path is already
    // hermetic (boot_team/tick use <catalog>/exec directly); this extends that isolation to spawned
    // sub-processes so a sub-`st2 up --host X` can't see or reap another eval's or the fleet's exec tasks.
    unsafe { std::env::set_var("XDG_STATE_HOME", catalog.join("state")) };

    let task_context = TaskCompileContext::current(catalog.clone())?;

    // Keep authored bare `st2` commands self-consistent with the runner. Generated DING sidecars use
    // the exact executable captured in `task_context` and never consult PATH.
    if let Some(dir) = task_context.st2_executable().parent() {
        let path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", format!("{}:{path}", dir.display())) };
    }

    let result = if EVAL_INTERRUPTED.load(Ordering::SeqCst) {
        Err(anyhow::anyhow!("eval interrupted by SIGINT/SIGTERM"))
    } else {
        run_eval_inner(&spec, &eval, &spec_dir, &catalog, &host, &task_context)
    };
    reap_all_eval_sessions(&catalog, &host)?;
    // Seats are already torn down inside run_eval_inner (no leaks). `--keep` preserves the catalog
    // files (worker repo base..HEAD, judge outputs, bus) for post-run inspection — e.g. a gate
    // reproduction reading the folder before it's "real"; otherwise the hermetic catalog is removed.
    if keep {
        eval_log!("catalog preserved (--keep): {}", catalog.display());
    } else {
        let _ = std::fs::remove_dir_all(&catalog);
    }
    result
}

fn reap_all_eval_sessions_with_runner<R: Runner>(runner: &R, host: &str) -> Result<()> {
    let _host = host;
    let mut last_error = None;
    for _ in 0..5 {
        let sessions = runner
            .list_sessions()
            .with_context(|| format!("listing eval sessions for host {host}"))?;
        if sessions.is_empty() {
            return Ok(());
        }
        for session in sessions {
            if session.alive
                && let Err(error) = runner.kill(&session.pty_id)
            {
                last_error = Some(format!("kill {}: {error:#}", session.pty_id));
            }
            if let Err(error) = runner.remove(&session.pty_id) {
                last_error = Some(format!("remove {}: {error:#}", session.pty_id));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!(
        "eval session reap did not reach empty state on host {host}; last error: {}",
        last_error.unwrap_or_else(|| "none".into())
    )
}

fn reap_all_eval_sessions(catalog: &Path, host: &str) -> Result<()> {
    let runner = SystemRunner::new(catalog.to_path_buf(), catalog.join("exec"));
    reap_all_eval_sessions_with_runner(&runner, host)
}

/// Idempotent safety net for eval catalog lifetime. Normal teardown remains responsible for
/// sessions; this guard ensures an unwind cannot strand the hermetic catalog on disk.
struct EvalCleanupGuard<R: Runner> {
    runner: R,
    catalog: PathBuf,
    host: String,
    keep: bool,
}
impl<R: Runner> Drop for EvalCleanupGuard<R> {
    fn drop(&mut self) {
        let reap = reap_all_eval_sessions_with_runner(&self.runner, &self.host);
        if let Err(error) = reap {
            tracing::warn!(
                "st2 eval cleanup: {error:#}; preserving catalog {}",
                self.catalog.display()
            );
            return;
        }
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.catalog);
        }
    }
}

/// Run the eval's `run { }` stage: its command steps, sequentially (declaration order), BEFORE judging.
/// Each step is `sh -c <command>` with cwd = its `workspace` in the catalog and env = the top-level
/// cascade + per-step `env` − `unset` (plus `$CATALOG`, `$RUNS_DIR`, and every earlier step's
/// `$RUN_<id>_EXIT`), retried on non-zero per its policy. stdout/stderr/exit are captured to
/// `$CATALOG/.runs/<id>.{out,err,exit}` for the judges to read. By DEFAULT a non-zero final exit
/// contributes a FAILING synthetic judge result (a step must succeed); an `allow-nonzero` step opts out,
/// leaving the exit for a judge to assert. (No per-step timeout yet: these are deterministic terminating
/// commands; a hard timeout is a follow-up seam.)
fn run_steps(
    steps: &[RunStep],
    catalog: &Path,
    top_env: &BTreeMap<String, String>,
) -> (Vec<JudgeResult>, BTreeMap<String, String>) {
    use std::process::{Command, Stdio};
    let mut results = Vec::new();
    // The env the JUDGES also get: $RUNS_DIR + each step's $RUN_<id>_EXIT (so a bash judge can read the
    // captures). Empty when there are no run steps.
    let mut judge_env: BTreeMap<String, String> = BTreeMap::new();
    if steps.is_empty() {
        return (results, judge_env);
    }
    let runs_dir = catalog.join(".runs");
    let _ = std::fs::create_dir_all(&runs_dir);
    judge_env.insert("RUNS_DIR".to_string(), runs_dir.display().to_string());
    // Unified, judge-greppable command logs: `<catalog>/logs/<label>.log` (same dir the exec sidecars
    // auto-log to). Judges get `$LOGS_DIR` so a judge can review/assert a run step's output by log.
    let logs_dir = catalog.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    judge_env.insert("LOGS_DIR".to_string(), logs_dir.display().to_string());
    let mut runtime: BTreeMap<String, String> = BTreeMap::new(); // RUN_<id>_EXIT, accumulated across steps

    for step in steps {
        // Effective env: the cascade + per-step override − unset. Values are `$CATALOG`-expanded.
        let mut env = top_env.clone();
        env.extend(step.env.clone());
        for u in &step.unset {
            env.remove(u);
        }
        let cwd = match &step.workspace {
            Some(w) => catalog.join(expand_catalog(w, catalog)),
            None => catalog.to_path_buf(),
        };
        let (attempts, backoff) = step
            .retry
            .as_ref()
            .map(|r| (r.attempts.max(1), r.delay))
            .unwrap_or((1, Duration::ZERO));

        let mut exit = -1;
        let out_path = runs_dir.join(format!("{}.out", step.id));
        let err_path = runs_dir.join(format!("{}.err", step.id));
        for attempt in 0..attempts {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(&step.command)
                .current_dir(&cwd)
                .env("CATALOG", catalog)
                .env("RUNS_DIR", &runs_dir);
            for (k, v) in &env {
                cmd.env(k, expand_catalog(v, catalog));
            }
            for (k, v) in &runtime {
                cmd.env(k, v);
            }
            // The child's stdout/stderr stream STRAIGHT into the capture files: a run step can emit
            // arbitrarily much output and it must never be buffered wholesale in this process. Each
            // attempt re-truncates, so the files hold the LAST attempt's output — the same bytes the
            // old buffer-then-write path produced.
            let attempt_exit = (|| -> std::io::Result<i32> {
                let out_file = std::fs::File::create(&out_path)?;
                let err_file = std::fs::File::create(&err_path)?;
                cmd.stdout(Stdio::from(out_file))
                    .stderr(Stdio::from(err_file));
                Ok(cmd.status()?.code().unwrap_or(-1))
            })();
            match attempt_exit {
                Ok(code) => exit = code,
                Err(e) => {
                    exit = -1;
                    let _ = std::fs::write(&err_path, format!("run step spawn failed: {e}"));
                }
            }
            if exit == 0 {
                break; // success — no more retries
            }
            if attempt + 1 < attempts {
                std::thread::sleep(backoff);
            }
        }

        let _ = std::fs::write(runs_dir.join(format!("{}.exit", step.id)), exit.to_string());
        // Also a unified, judge-greppable combined log (stdout then stderr), copied from the two
        // capture files so neither ever has to fit in memory.
        let combined_path = logs_dir.join(format!("{}.log", step.id));
        let _ = (|| -> std::io::Result<()> {
            let mut log = std::fs::File::create(&combined_path)?;
            let mut out = std::fs::File::open(&out_path)?;
            std::io::copy(&mut out, &mut log)?;
            let mut err = std::fs::File::open(&err_path)?;
            std::io::copy(&mut err, &mut log)?;
            Ok(())
        })();
        runtime.insert(format!("RUN_{}_EXIT", env_key(&step.id)), exit.to_string());
        eval_log!(
            "== run step {} → exit {}{} ==",
            step.id,
            exit,
            if step.allow_nonzero {
                " (allow-nonzero)"
            } else {
                ""
            }
        );

        if !step.allow_nonzero {
            // Default: a run step must succeed. A non-zero final exit hard-fails the verdict as a
            // gating synthetic judge. `allow-nonzero` steps skip this — the exit is the judges' to assert.
            results.push(JudgeResult {
                name: format!("step:{} exit 0", step.id),
                passed: exit == 0,
                detail: format!("exit {exit}"),
                signal: false, // a must-succeed step GATES the verdict
            });
        }
    }
    // Hand the judges $RUNS_DIR + every $RUN_<id>_EXIT.
    judge_env.extend(runtime);
    (results, judge_env)
}

/// Snapshot each PTY task's terminal output to `<catalog>/logs/<id>.log` (plain-text full scrollback via
/// `pty peek`), so judges can review/assert an agent's output by log. Best-effort: `pty` has no
/// continuous plain-text log, so this is the scrollback captured at judge time — enough to inspect a
/// wedged/finished agent's history. A truly continuous agent log would need a `pty` feature.
fn dump_agent_logs(pty_task_ids: &[String], catalog: &Path) {
    use std::process::Stdio;
    if pty_task_ids.is_empty() {
        return;
    }
    let logs_dir = catalog.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    let pty_root = crate::run::effective_pty_root(catalog);
    for task_id in pty_task_ids {
        let log_path = logs_dir.join(format!("{task_id}.log"));
        // Stream the peek's stdout straight into the log file: the full scrollback can be large and
        // must never be buffered wholesale in this process. The old contract — keep the log only
        // when the peek succeeded AND produced output — is kept by deleting the (possibly empty)
        // file otherwise.
        let dumped = (|| -> std::io::Result<bool> {
            let mut child = std::process::Command::new("pty")
                .args(["peek", "--full", "--plain", task_id])
                .env("PTY_ROOT", &pty_root)
                .stdout(Stdio::from(std::fs::File::create(&log_path)?))
                .stderr(Stdio::null())
                .spawn()?;
            child.wait().map(|s| s.success())
        })();
        let keep =
            dumped.unwrap_or(false) && std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 0);
        if !keep {
            let _ = std::fs::remove_file(&log_path);
        }
    }
}

/// An env-var-safe form of a step id (non-alphanumerics → `_`), for `RUN_<id>_EXIT`.
fn env_key(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The supervisor chain of `agent_id`, walked transitively via each agent's `supervisor` field to the
/// root (whose supervisor is `None` — the cos). Returns the ancestor ids, nearest first. A cycle or a
/// supervisor that names no declared agent terminates the walk (the named id is still included — we ding
/// its inbox regardless of whether it has a running task).
fn supervisor_chain(agent_id: &str, specs: &[AgentSpec], host: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let find = |identity: &str| {
        specs
            .iter()
            .find(|spec| spec.identity == identity || spec.bus_id(host) == identity)
    };
    let mut current = find(agent_id).and_then(|s| s.supervisor.clone());
    while let Some(sup) = current {
        if !seen.insert(sup.clone()) {
            break; // cycle guard
        }
        chain.push(sup.clone());
        current = find(&sup).and_then(|s| s.supervisor.clone());
    }
    chain
}

/// Emit a crash-ding for a crashed task to every ancestor in its owning agent's supervisor chain.
fn crash_ding(
    agent_id: &str,
    task_id: &str,
    specs: &[AgentSpec],
    bus: &Path,
    host: &str,
    canonical_routes: Option<&BTreeMap<String, CanonicalRoute>>,
) {
    let chain = supervisor_chain(agent_id, specs, host);
    if chain.is_empty() {
        return;
    }
    let subject = format!("worker crash: {task_id}");
    let body = format!(
        "Agent task '{task_id}' crashed — its session died non-cleanly (non-zero exit / killed / vanished). \
         st2 respawned it from spec; surfacing the crash up the supervision chain."
    );
    for ancestor in &chain {
        let inbox = match canonical_routes {
            Some(routes) => admitted_route(routes, ancestor).inbox.clone(),
            None => bus.join(ancestor).join("inbox"),
        };
        let _ = crate::message::send_to_inbox(&inbox, "st2", Some(&subject), None, &[], &body);
        eval_log!("== crash-ding: {task_id} → {ancestor} ==");
    }
}

fn run_eval_inner(
    spec: &Spec,
    eval: &Eval,
    spec_dir: &Path,
    catalog: &Path,
    host: &str,
    task_context: &TaskCompileContext,
) -> Result<EvalReport> {
    // Copy the fixture's CONTENTS into the catalog root (the start world), _git → .git.
    if let Some(copy) = &eval.copy {
        let src = spec_dir.join(copy);
        copy_tree(&src, catalog).with_context(|| format!("copying fixture {}", src.display()))?;
    }

    let bus = bus_root(spec, catalog);
    let requester = eval
        .message
        .as_ref()
        .map(|m| m.from.clone())
        .unwrap_or_else(|| "eval-runner".to_string());

    // The run{} stage runs to completion BEFORE judging — the WHOLE work of a team-less eval, or setup
    // before a team. must-exit-0 failures come back as failing synthetic judge results (folded into
    // the verdict); `run_env` ($RUNS_DIR + each $RUN_<id>_EXIT) is handed to the judges to read captures.
    let (mut judges, run_env) = run_steps(&eval.run_steps, catalog, &spec.env);

    // The base team + eval-only compact agents. `canonical-agents` is a mutually exclusive authority:
    // it discovers the post-run hermetic catalog rather than projecting this compact grammar.
    let mut compact_agents = spec.agents.clone();
    compact_agents.extend(eval.agents.clone());

    let (done, specs, pty_task_ids) = if compact_agents.is_empty() && !eval.canonical_agents {
        // TEAM-LESS: nothing to boot, kick off, or wait on — the run steps did the work → straight to judging.
        if !eval.run_steps.is_empty() {
            eval_log!(
                "== team-less eval: {} run step(s) ran → judging ==",
                eval.run_steps.len()
            );
        }
        (true, Vec::new(), Vec::new())
    } else {
        let (mut specs, participant_ids, canonical_routes) = if eval.canonical_agents {
            if bus != catalog {
                anyhow::bail!(
                    "canonical-agents requires the native flat ST_ROOT `{}`, got `{}`",
                    catalog.display(),
                    bus.display()
                );
            }
            let team = load_canonical_eval_team(catalog, host)?;
            let participants = team
                .specs
                .iter()
                .map(|spec| spec.bus_id(host))
                .collect::<Vec<_>>();
            (team.specs, participants, Some(team.routes))
        } else {
            let specs = spec_to_agent_specs(&compact_agents, host, catalog);
            let participants = specs
                .iter()
                .map(|spec| spec.bus_id(host))
                .collect::<Vec<_>>();
            (specs, participants, None)
        };
        compile_generated_tasks(&mut specs, host, task_context)?;
        let runtime_tasks = eval_runtime_tasks(&specs, host);
        let task_ids = runtime_tasks
            .iter()
            .map(|task| task.runtime_id.clone())
            .collect::<Vec<_>>();
        let pty_task_ids = runtime_tasks
            .iter()
            .filter(|task| task.is_pty)
            .map(|task| task.runtime_id.clone())
            .collect::<Vec<_>>();
        if eval.supervise && !eval.canonical_agents {
            add_eval_exit_markers(&mut specs, catalog, host);
        }
        if !eval.canonical_agents {
            // Compact legacy agents intentionally retain their historical ambient trust behavior.
            // Canonical managed Agent Specs own trust inside their declared adapter trajectory.
            let dirs: Vec<PathBuf> = compact_agents
                .iter()
                .filter_map(|a| a.workspace.as_deref())
                .map(|w| catalog.join(w))
                .collect();
            if !dirs.is_empty() {
                let _ = crate::pretrust::pretrust(&dirs);
            }
        }
        let msg = eval.message.as_ref().ok_or_else(|| {
            anyhow::anyhow!("a team eval needs a message{{}} kickoff before any agent can launch")
        })?;
        let matches = specs
            .iter()
            .filter(|agent| agent.identity == msg.to || agent.bus_id(host) == msg.to)
            .map(|agent| agent.bus_id(host))
            .collect::<Vec<_>>();
        let [sup] = matches.as_slice() else {
            let authority = if eval.canonical_agents {
                "canonical-agents"
            } else {
                "compact"
            };
            anyhow::bail!(
                "{authority} kickoff target `{}` must resolve to exactly one Agent Spec, found {}",
                msg.to,
                matches.len()
            );
        };
        let sup = sup.clone();

        if let Some(routes) = canonical_routes.as_ref() {
            if routes.contains_key(&requester) {
                anyhow::bail!(
                    "canonical-agents requester `{requester}` must be external to the admitted Agent Specs"
                );
            }
            crate::message::ExternalInbox::provision(&bus, &requester)?;
            for spec in &mut specs {
                for task in &mut spec.tasks {
                    task.env
                        .insert("ST2_EVAL_REQUESTER".to_owned(), requester.clone());
                }
            }
        }

        eval_log!("== boot team ({} agents) ==", specs.len());
        let boot = boot_team(&specs, host, catalog)?;
        if eval.canonical_agents {
            require_canonical_boot(&boot, &task_ids)?;
        }
        boot_gate(&task_ids, &specs, host, catalog)?;

        // Deliver the kickoff onto the bus the agents' DING tasks watch (ST_ROOT), from the requester.
        let body = resolve_content(&msg.content, spec_dir)?;
        let to_inbox = match canonical_routes.as_ref() {
            Some(routes) => admitted_route(routes, &sup).inbox.clone(),
            None => bus.join(&sup).join("inbox"),
        };
        let requester_before_kickoff = eval.canonical_agents.then(|| {
            crate::message::list_dir(&bus.join(&msg.from).join("inbox"))
                .unwrap_or_default()
                .into_iter()
                .map(|message| message.filename)
                .collect::<HashSet<_>>()
        });
        let kickoff_receipt =
            crate::message::send_to_inbox(&to_inbox, &msg.from, None, None, &[], &body)
                .with_context(|| format!("seeding kickoff into {}", to_inbox.display()))?;
        let kickoff_ts = eval
            .canonical_agents
            .then(|| message_timestamp(&kickoff_receipt))
            .transpose()?;
        eval_log!("== kickoff → {sup} (from {}) ==", msg.from);

        let workers: Vec<String> = participant_ids
            .into_iter()
            .filter(|id| *id != sup)
            .collect();
        eval_log!(
            "== waiting for {sup}→{} confirmation post-dating a worker report (≤{:?}) ==",
            msg.from,
            eval.max_timeout
        );

        // `supervise`: each wait tick, respawn any dead task FROM SPEC (full env → rejoins cold). Carry a
        // FlappingCap + LivenessDebounce ACROSS ticks so a fault-injected kill is respawned exactly ONCE
        // (cap rate-limits; debounce absorbs a transient `pty list` misread). Scoped so the tick's borrow
        // of `specs` ends before we hand `specs` to teardown.
        let done = {
            let supervise_runner = SystemRunner::new(catalog.to_path_buf(), catalog.join("exec"));
            let mut sup_cap = crate::flapping::FlappingCap::default();
            let mut sup_debounce = crate::run::LivenessDebounce::new(Duration::from_secs(2));
            let mut sup_presentation_cursor = crate::run::PresentationPatchCursor::default();
            // Crash-ding state: the boot gate immediately above proved every declared task alive, so
            // carry that proof into supervision. PTY may self-reap a fast exit before the first
            // post-kickoff list snapshot; starting empty would then misclassify the proven-live task
            // as never booted and suppress its crash ding. `dinged` dedups so one crash = one ding
            // until the task is alive again.
            let mut ever_alive: std::collections::HashSet<String> =
                task_ids.iter().cloned().collect();
            let mut dinged: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut tick = || {
                if eval.supervise {
                    // Detect crashes BEFORE respawn (reconcile reaps the dead session): a declared task
                    // that was alive and is now dead non-cleanly (non-zero/killed/vanished) → crash-ding
                    // its supervisor chain. A clean exit (code 0) stays SILENT (a false ding on a routine
                    // finish is as bad as a missed crash).
                    let report = match supervised_eval_sessions(&specs, host, &supervise_runner) {
                        Ok(sessions) => {
                            let by_id: std::collections::HashMap<&str, &crate::reconcile::Session> =
                                sessions.iter().map(|s| (s.pty_id.as_str(), s)).collect();
                            for task in &runtime_tasks {
                                let id = task.runtime_id.as_str();
                                match by_id.get(id) {
                                    Some(s) if s.alive => {
                                        ever_alive.insert(id.to_string());
                                        dinged.remove(id); // healthy again → re-arm for a future crash
                                    }
                                    found => {
                                        let clean = matches!(found, Some(s) if s.exit_code == Some(0))
                                            || eval_exit_code(catalog, id) == Some(0);
                                        if ever_alive.contains(id) && !clean && !dinged.contains(id)
                                        {
                                            crash_ding(
                                                &task.agent_id,
                                                id,
                                                &specs,
                                                &bus,
                                                host,
                                                canonical_routes.as_ref(),
                                            );
                                            dinged.insert(id.to_string());
                                        }
                                    }
                                }
                            }
                            // Use the SAME snapshot for reconciliation. A second list here could observe
                            // and reap a just-cleanly-exited seat before the classifier records code 0.
                            crate::run::reconcile_pass_specs_with_sessions(
                                &specs,
                                &sessions,
                                host,
                                &supervise_runner,
                                &mut sup_cap,
                                &mut sup_debounce,
                                &mut sup_presentation_cursor,
                            )
                        }
                        Err(_) => crate::run::reconcile_pass_specs_with_cursor(
                            &specs,
                            host,
                            &supervise_runner,
                            &mut sup_cap,
                            &mut sup_debounce,
                            &mut sup_presentation_cursor,
                        ),
                    };
                    if !report.launched.is_empty() {
                        eval_log!("== supervise: launched {:?} from spec ==", report.launched);
                    }
                    if !report.restarted.is_empty() {
                        eval_log!(
                            "== supervise: restarted {:?} from spec ==",
                            report.restarted
                        );
                    }
                }
            };
            wait_done(
                &bus,
                canonical_routes.as_ref(),
                &sup,
                &msg.from,
                &workers,
                kickoff_ts,
                requester_before_kickoff.as_ref(),
                eval.max_timeout,
                &mut tick,
            )
        };

        if EVAL_INTERRUPTED.load(Ordering::SeqCst) {
            anyhow::bail!("eval interrupted by SIGINT/SIGTERM");
        }

        if done {
            eval_log!("== team signalled done — judging ==");
        } else {
            eval_log!(
                "== max-timeout: no confirmation within {:?} — judging the final state ==",
                eval.max_timeout
            );
        }
        (done, specs, pty_task_ids)
    };

    if eval.canonical_agents {
        judges.push(JudgeResult {
            name: "canonical team completion".to_string(),
            passed: done,
            detail: if done {
                "post-kickoff completion received".to_string()
            } else {
                "no post-kickoff completion before max-timeout".to_string()
            },
            signal: false,
        });
    }

    // Snapshot each agent's terminal output to logs/<id>.log so judges can review/assert an agent's
    // output by log (alongside the run-step + exec sidecar logs already there). Done BEFORE teardown so
    // the sessions are still peekable.
    dump_agent_logs(&pty_task_ids, catalog);

    // Judges: the run-step gate results first, then the declared judges (all must pass). Judge BEFORE
    // teardown — an ask-agent judge needs its judge agent still alive to answer.
    judges.extend(run_judges(
        &eval.judges,
        spec_dir,
        catalog,
        &bus,
        &requester,
        &run_env,
    ));
    // Under `supervise`, reap runtime-spawned tasks too (team-standup), not just the declared team.
    teardown_team(&specs, host, catalog, eval.supervise);
    Ok(EvalReport {
        done,
        judges,
        timeout: eval.max_timeout,
    })
}

// ── P4: the judge engine (all-must-pass; declarative / bash / ask-agent; per-judge timeout) ─────────

/// Run every judge in declared order (NO short-circuit — a full legible report). Each judge is one
/// flavor with an optional per-judge timeout (default 120s). `run_env` ($RUNS_DIR + each $RUN_<id>_EXIT)
/// is exported into bash judges so they can read the run steps' captured stdout/stderr/exit.
pub fn run_judges(
    judges: &[Judge],
    spec_dir: &Path,
    catalog: &Path,
    bus: &Path,
    requester: &str,
    run_env: &BTreeMap<String, String>,
) -> Vec<JudgeResult> {
    let default_timeout = Duration::from_secs(120);
    judges
        .iter()
        .map(|j| {
            let timeout = j.timeout.unwrap_or(default_timeout);
            let (passed, detail) = match &j.kind {
                JudgeKind::Declarative(checks) => run_declarative(checks, catalog),
                JudgeKind::Bash(cmd) => {
                    run_bash_judge(cmd, spec_dir, catalog, bus, timeout, run_env)
                }
                JudgeKind::Ask { agent, prompt } => {
                    run_ask_judge(agent, prompt, bus, requester, timeout)
                }
            };
            JudgeResult {
                name: j.name.clone(),
                passed,
                detail,
                signal: j.signal,
            }
        })
        .collect()
}

/// All declarative checks must hold. Paths are relative to the catalog root (the copied world).
fn run_declarative(checks: &[Check], catalog: &Path) -> (bool, String) {
    for c in checks {
        let (ok, why) = match c {
            Check::FileHas { path, text } => {
                let body = std::fs::read_to_string(catalog.join(path)).unwrap_or_default();
                (body.contains(text.as_str()), format!("{path} has {text:?}"))
            }
            Check::FileLacks { path, text } => {
                let body = std::fs::read_to_string(catalog.join(path)).unwrap_or_default();
                (
                    !body.contains(text.as_str()),
                    format!("{path} lacks {text:?}"),
                )
            }
            Check::JsonField { path, field, value } => {
                let expected = match value {
                    crate::eval_spec::JsonScalar::String(s) => serde_json::Value::String(s.clone()),
                    crate::eval_spec::JsonScalar::Bool(b) => serde_json::Value::Bool(*b),
                    crate::eval_spec::JsonScalar::Integer(i) => {
                        serde_json::Value::Number((*i).into())
                    }
                };
                let got = std::fs::read_to_string(catalog.join(path))
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v.get(field).cloned());
                (
                    got.as_ref() == Some(&expected),
                    format!("{path} field {field} is {value:?} (got {got:?})"),
                )
            }
            Check::Committed { path } => {
                let ok = is_committed_clean(catalog, path);
                (ok, format!("{path} committed + clean"))
            }
        };
        if !ok {
            return (false, format!("FAILED: {why}"));
        }
    }
    (true, format!("{} check(s) passed", checks.len()))
}

/// `committed "p"`: the path is tracked AND has no uncommitted changes, in the repo that owns it.
fn is_committed_clean(catalog: &Path, path: &str) -> bool {
    // Find the repo root (nearest ancestor with a `.git`) of the target.
    let target = catalog.join(path);
    let mut repo = target.parent();
    while let Some(dir) = repo {
        if dir.join(".git").exists() {
            let rel = target.strip_prefix(dir).unwrap_or(&target);
            let tracked = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["ls-files", "--error-unmatch"])
                .arg(rel)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let clean = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["status", "--porcelain", "--"])
                .arg(rel)
                .output()
                .map(|o| o.stdout.is_empty())
                .unwrap_or(false);
            return tracked && clean;
        }
        repo = dir.parent();
    }
    false
}

/// A bash judge: `sh -c <cmd>` with CWD = the SPEC FOLDER (so `./judges/x.sh` resolves — judge scripts
/// live beside the spec and are NOT copied into the catalog), while the sandbox/world is reached via
/// `$CATALOG` and the bus via `$ST_ROOT` (both exported; `$SPEC_DIR` too for an explicit reference).
/// Exit 0 = pass; `bash …`/shebangs honored (no forced POSIX sh). Bounded by the per-judge timeout.
fn run_bash_judge(
    cmd: &str,
    spec_dir: &Path,
    catalog: &Path,
    bus: &Path,
    timeout: Duration,
    run_env: &BTreeMap<String, String>,
) -> (bool, String) {
    use std::process::{Command, Stdio};
    // `sh` reports the physical cwd on macOS (for example `/private/var/...`) even when tempfile
    // handed us its symlinked spelling (`/var/...`). Export the same physical path so `$SPEC_DIR`
    // remains a truthful explicit name for the judge's cwd on every platform.
    let physical_spec_dir = spec_dir
        .canonicalize()
        .unwrap_or_else(|_| spec_dir.to_path_buf());
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(&physical_spec_dir)
        .env("CATALOG", catalog)
        .env("ST_ROOT", bus)
        .env("SPEC_DIR", &physical_spec_dir)
        // Only the status/exit detail is consumed. Piping stdout/stderr and never draining them
        // deadlocks a chatty judge: once its pipe buffers fill, the child blocks forever while the
        // parent keeps polling try_wait.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // $RUNS_DIR + each $RUN_<id>_EXIT, so a judge can read the run steps' captured stdout/stderr/exit.
    for (k, v) in run_env {
        command.env(k, v);
    }
    let child = command.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("spawn failed: {e}")),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return (
                    status.success(),
                    format!("exit {}", status.code().unwrap_or(-1)),
                );
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    return (false, format!("timed out after {timeout:?}"));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return (false, format!("wait failed: {e}")),
        }
    }
}

/// An ask-agent judge: message the judge agent the prompt, wait for its reply (post the ask) in the
/// requester's inbox within the timeout, and read PASS/FAIL out of it.
fn run_ask_judge(
    agent: &str,
    prompt: &str,
    bus: &Path,
    requester: &str,
    timeout: Duration,
) -> (bool, String) {
    let ask_ts = now_ms();
    let to_inbox = bus.join(agent).join("inbox");
    if let Err(e) =
        crate::message::send_to_inbox(&to_inbox, requester, Some("judge"), None, &[], prompt)
    {
        return (false, format!("could not ask judge '{agent}': {e}"));
    }
    let req_inbox = bus.join(requester).join("inbox");
    let deadline = Instant::now() + timeout;
    loop {
        // The judge's reply = a message from <agent> in the requester's inbox at/after the ask.
        if let Some(reply) = crate::message::list_dir(&req_inbox)
            .unwrap_or_default()
            .into_iter()
            .find(|m| from_is(m.from.as_deref(), agent) && m.ts_ms >= ask_ts)
        {
            let body = std::fs::read_to_string(req_inbox.join(&reply.filename)).unwrap_or_default();
            return match parse_pass_fail(&body) {
                Some(true) => (true, "judge replied PASS".to_string()),
                Some(false) => (false, "judge replied FAIL".to_string()),
                None => (false, "judge reply had no PASS/FAIL".to_string()),
            };
        }
        if Instant::now() > deadline {
            return (
                false,
                format!("judge '{agent}' did not reply within {timeout:?}"),
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Read PASS/FAIL from a judge reply: whichever of `PASS`/`FAIL` appears FIRST wins (case-insensitive).
fn parse_pass_fail(reply: &str) -> Option<bool> {
    let up = reply.to_uppercase();
    match (up.find("PASS"), up.find("FAIL")) {
        (Some(p), Some(f)) => Some(p < f),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

/// Current unix-ms (used to bound an ask reply to messages that post-date the ask).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::{Session, TaskTarget};
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::PermissionsExt as _;

    fn write_eval_agent(catalog: &Path, relative: &str, body: &str) {
        let path = catalog.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn canonical_eval_team_uses_exact_catalog_declarations_and_runtime_ids() {
        let catalog = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(catalog.path().join("sup")).unwrap();
        std::fs::create_dir_all(catalog.path().join("worker")).unwrap();
        write_eval_agent(
            catalog.path(),
            "agents/evalhost/sup/agent.kdl",
            r#"agent "sup" {
  identity "sup"
  host "evalhost"
  workspace "$CATALOG/sup"
  argv "sh" "-c" "sleep 60"
}
"#,
        );
        write_eval_agent(
            catalog.path(),
            "agents/evalhost/worker/agent.kdl",
            r#"agent "worker" {
  identity "worker"
  host "evalhost"
  supervisor "evalhost.sup"
  workspace "$CATALOG/worker"
  argv "sh" "-c" "sleep 60"
}
"#,
        );

        let team = load_canonical_eval_team(catalog.path(), "evalhost").unwrap();
        assert_eq!(team.specs.len(), 2);
        let context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
        let mut compiled = team.specs.clone();
        compile_generated_tasks(&mut compiled, "evalhost", &context).unwrap();
        assert_eq!(
            eval_runtime_tasks(&compiled, "evalhost")
                .iter()
                .map(|task| task.runtime_id.as_str())
                .collect::<Vec<_>>(),
            ["evalhost.sup", "evalhost.worker"]
        );
        assert!(
            team.specs
                .iter()
                .all(|spec| spec.path.ends_with("agent.kdl"))
        );
    }

    #[test]
    fn canonical_eval_runtime_inventory_compiles_driver_launches() {
        let catalog = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(catalog.path().join("worker")).unwrap();
        // The Claude driver expansion registers `$ST_HOOKS` hooks, and materialization refuses an
        // unverified hook set — the same contract an eval host satisfies with `st2 hooks install`.
        // The root outlives the test (env stays set for the process), so it is kept, not dropped.
        let hooks = tempfile::tempdir().unwrap().keep();
        crate::hooks::install_at(&hooks, false).unwrap();
        unsafe { std::env::set_var("ST_HOOKS", &hooks) };
        write_eval_agent(
            catalog.path(),
            "agents/evalhost/worker/agent.kdl",
            r#"agent "worker" {
  host "evalhost"
  workspace "$CATALOG/worker"
  claude { prompt "Start the assigned work." }
  delivery-readiness "credential"
}
"#,
        );

        let team = load_canonical_eval_team(catalog.path(), "evalhost").unwrap();
        assert!(!team.specs[0].is_runnable());
        let context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
        let mut compiled = team.specs.clone();
        compile_generated_tasks(&mut compiled, "evalhost", &context).unwrap();

        assert!(compiled[0].is_runnable());
        assert_eq!(
            eval_runtime_tasks(&compiled, "evalhost"),
            [EvalRuntimeTask {
                agent_id: "evalhost.worker".into(),
                runtime_id: "evalhost.worker".into(),
                is_pty: true,
            }]
        );
    }

    #[test]
    fn canonical_eval_team_projects_local_path_independent_agents_and_tears_down_every_task() {
        let catalog = tempfile::tempdir().unwrap();
        write_eval_agent(
            catalog.path(),
            "organization/.managed/arbitrary/declaration/agent.kdl",
            r#"agent "local" {
  identity "local"
  host "evalhost"
  pty "work" { id "local-work"; command "sleep 60" }
  exec "watch" { id "local-watch"; command "sleep 60" }
}"#,
        );
        write_eval_agent(
            catalog.path(),
            "fleet/remote/declaration/agent.kdl",
            r#"agent "remote" {
  identity "remote"
  host "other"
  pty "remote-work" { id "remote-work"; command "sleep 60" }
}"#,
        );

        let team = load_canonical_eval_team(catalog.path(), "evalhost").unwrap();
        assert_eq!(
            team.specs
                .iter()
                .map(|spec| spec.bus_id("evalhost"))
                .collect::<Vec<_>>(),
            ["evalhost.local"]
        );
        assert_eq!(
            {
                let context = TaskCompileContext::current(catalog.path().to_path_buf()).unwrap();
                let mut compiled = team.specs.clone();
                compile_generated_tasks(&mut compiled, "evalhost", &context).unwrap();
                eval_runtime_tasks(&compiled, "evalhost")
            },
            [
                EvalRuntimeTask {
                    agent_id: "evalhost.local".into(),
                    runtime_id: "local-watch".into(),
                    is_pty: false,
                },
                EvalRuntimeTask {
                    agent_id: "evalhost.local".into(),
                    runtime_id: "local-work".into(),
                    is_pty: true,
                },
            ]
        );
        assert_eq!(
            admitted_route(&team.routes, "evalhost.local").inbox,
            catalog
                .path()
                .join("organization/.managed/arbitrary/declaration/resources/inbox")
        );

        struct RecordingRunner {
            sessions: Vec<Session>,
            killed: RefCell<Vec<String>>,
        }
        impl Runner for RecordingRunner {
            fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
                Ok(self.sessions.clone())
            }
            fn spawn(&self, _: &TaskTarget, _: &Path) -> anyhow::Result<()> {
                unreachable!("teardown must not spawn")
            }
            fn kill(&self, id: &str) -> anyhow::Result<()> {
                self.killed.borrow_mut().push(id.to_string());
                Ok(())
            }
            fn remove(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let runner = RecordingRunner {
            sessions: ["local-work", "local-watch", "remote-work"]
                .into_iter()
                .map(|id| Session {
                    pty_id: id.into(),
                    alive: true,
                    exit_code: None,
                    presentation: None,
                })
                .collect(),
            killed: RefCell::new(Vec::new()),
        };
        teardown_team_with_runner(&team.specs, "evalhost", &runner, false);
        let mut killed = runner.killed.into_inner();
        killed.sort();
        assert_eq!(killed, ["local-watch", "local-work"]);
    }

    #[test]
    fn canonical_eval_team_fails_closed_before_launch_on_zero_or_duplicate_specs() {
        let empty = tempfile::tempdir().unwrap();
        assert!(
            load_canonical_eval_team(empty.path(), "evalhost")
                .unwrap_err()
                .to_string()
                .contains("no local canonical Agent Specs")
        );

        let duplicate = tempfile::tempdir().unwrap();
        write_eval_agent(
            duplicate.path(),
            "agents/evalhost/worker/agent.kdl",
            r#"
agent "worker" { identity "worker"; host "evalhost"; argv "true" }
agent "worker" { identity "worker"; host "evalhost"; argv "true" }
"#,
        );
        let error = load_canonical_eval_team(duplicate.path(), "evalhost")
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate"), "{error}");
    }

    #[test]
    fn canonical_eval_team_applies_strict_validation_and_task_invariants() {
        let cases = [
            (
                "unknown-type",
                vec![(
                    "agents/evalhost/worker/agent.kdl",
                    r#"agent "worker" {
  identity "worker"
  host "evalhost"
  type "srvice"
  argv "true"
}"#,
                )],
            ),
            (
                "unknown-task-kind",
                vec![(
                    "agents/evalhost/worker/agent.kdl",
                    r#"agent "worker" {
  identity "worker"
  host "evalhost"
  argv "true"
  pty { command "true" }
}"#,
                )],
            ),
            (
                "supervisor-missing",
                vec![(
                    "agents/evalhost/worker/agent.kdl",
                    r#"agent "worker" {
  identity "worker"
  host "evalhost"
  supervisor "missing"
  argv "true"
}"#,
                )],
            ),
            (
                "bad-path",
                vec![(
                    "agents/evalhost/worker/agent.kdl",
                    r#"agent "worker" {
  identity "worker"
  host "evalhost"
  workspace "$CATALOG/missing"
  argv "true"
}"#,
                )],
            ),
            (
                "ST2_EVAL_REQUESTER",
                vec![(
                    "agents/evalhost/worker/agent.kdl",
                    r#"agent "worker" {
  identity "worker"
  host "evalhost"
  env { ST2_EVAL_REQUESTER "shadow-requester" }
  argv "true"
}"#,
                )],
            ),
            (
                "duplicate runtime task id",
                vec![
                    (
                        "agents/evalhost/one/agent.kdl",
                        r#"agent "one" {
  identity "one"
  host "evalhost"
  pty "agent" { id "shared"; command "sleep 60" }
}"#,
                    ),
                    (
                        "agents/evalhost/two/agent.kdl",
                        r#"agent "two" {
  identity "two"
  host "evalhost"
  supervisor "evalhost.one"
  pty "agent" { id "two-main"; command "sleep 60" }
  exec "poison" { id "shared"; command "true" }
}"#,
                    ),
                ],
            ),
        ];
        for (expected, specs) in cases {
            let catalog = tempfile::tempdir().unwrap();
            for (path, body) in specs {
                write_eval_agent(catalog.path(), path, body);
            }
            let error = load_canonical_eval_team(catalog.path(), "evalhost")
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "expected `{expected}` refusal, got: {error}"
            );
        }

        let warning = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(warning.path().join("workspace")).unwrap();
        write_eval_agent(
            warning.path(),
            "agents/evalhost/worker/agent.kdl",
            r#"agent "worker" {
  identity "worker"
  host "evalhost"
  workspace "$CATALOG/workspace"
  argv "true"
  render { git-exclude ".st2/" }
}"#,
        );
        let error = load_canonical_eval_team(warning.path(), "evalhost")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("materialization") && error.contains("git-exclude"),
            "materialization warning was not a pre-spawn failure: {error}"
        );
    }

    struct RaceRunner {
        lists: RefCell<Vec<Vec<Session>>>,
        ops: RefCell<Vec<String>>,
    }
    impl Runner for RaceRunner {
        fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            Ok(self.lists.borrow_mut().remove(0))
        }
        fn spawn(&self, _: &TaskTarget, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn kill(&self, id: &str) -> anyhow::Result<()> {
            self.ops.borrow_mut().push(format!("kill:{id}"));
            anyhow::bail!("already gone")
        }
        fn remove(&self, id: &str) -> anyhow::Result<()> {
            self.ops.borrow_mut().push(format!("remove:{id}"));
            anyhow::bail!("already gone")
        }
    }

    struct InventoryRunner {
        list_calls: Cell<usize>,
    }

    impl Runner for InventoryRunner {
        fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            self.list_calls.set(self.list_calls.get() + 1);
            Ok(Vec::new())
        }

        fn spawn(&self, _: &TaskTarget, _: &Path) -> anyhow::Result<()> {
            panic!("identity admission must refuse before spawn")
        }

        fn kill(&self, _: &str) -> anyhow::Result<()> {
            panic!("identity admission must refuse before kill")
        }

        fn remove(&self, _: &str) -> anyhow::Result<()> {
            panic!("identity admission must refuse before remove")
        }
    }

    fn conflicting_eval_specs() -> Vec<AgentSpec> {
        let parsed = parse_spec(r#"agent "worker" { command "true" }"#).unwrap();
        let mut specs = spec_to_agent_specs(&parsed.agents, "host", Path::new("/tmp/eval"));
        specs[0].tasks[0]
            .env
            .insert("ST_AGENT".into(), "wrong.actor".into());
        specs
    }

    #[test]
    fn eval_boot_identity_conflict_refuses_before_runner_inventory() {
        let runner = InventoryRunner {
            list_calls: Cell::new(0),
        };

        let error = boot_team_with_runner(&conflicting_eval_specs(), "host", &runner).unwrap_err();

        assert!(error.to_string().contains("conflicting ST_AGENT"));
        assert_eq!(runner.list_calls.get(), 0);
    }

    #[test]
    fn supervised_eval_tick_identity_conflict_refuses_before_runner_inventory() {
        let runner = InventoryRunner {
            list_calls: Cell::new(0),
        };

        let error =
            supervised_eval_sessions(&conflicting_eval_specs(), "host", &runner).unwrap_err();

        assert!(error.to_string().contains("conflicting ST_AGENT"));
        assert_eq!(runner.list_calls.get(), 0);
    }

    #[test]
    fn reap_race_errors_converge_only_after_empty_list() {
        let runner = RaceRunner {
            lists: RefCell::new(vec![
                vec![Session {
                    pty_id: "x".into(),
                    alive: true,
                    exit_code: None,
                    presentation: None,
                }],
                vec![],
            ]),
            ops: RefCell::new(Vec::new()),
        };
        assert!(reap_all_eval_sessions_with_runner(&runner, "test").is_ok());
        assert_eq!(runner.ops.borrow().len(), 2);
    }

    struct PersistentRunner {
        lists: RefCell<usize>,
        ops: RefCell<usize>,
    }
    impl Runner for PersistentRunner {
        fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            *self.lists.borrow_mut() += 1;
            Ok(vec![Session {
                pty_id: "stuck".into(),
                alive: true,
                exit_code: None,
                presentation: None,
            }])
        }
        fn spawn(&self, _: &TaskTarget, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            *self.ops.borrow_mut() += 1;
            Ok(())
        }
        fn remove(&self, _: &str) -> anyhow::Result<()> {
            *self.ops.borrow_mut() += 1;
            Ok(())
        }
    }

    #[test]
    fn reap_persistent_residual_fails_after_bounded_attempts() {
        let runner = PersistentRunner {
            lists: RefCell::new(0),
            ops: RefCell::new(0),
        };
        let error = reap_all_eval_sessions_with_runner(&runner, "host-x")
            .unwrap_err()
            .to_string();
        assert!(error.contains("host-x") && error.contains("empty state"));
        assert_eq!(*runner.lists.borrow(), 5);
        assert_eq!(*runner.ops.borrow(), 10);
    }

    #[test]
    fn cleanup_guard_reaps_on_unwind_without_double_panic() {
        use std::rc::Rc;
        let lists = Rc::new(RefCell::new(vec![
            vec![Session {
                pty_id: "panic".into(),
                alive: true,
                exit_code: None,
                presentation: None,
            }],
            vec![],
        ]));
        let ops = Rc::new(RefCell::new(Vec::new()));
        struct Shared {
            lists: Rc<RefCell<Vec<Vec<Session>>>>,
            ops: Rc<RefCell<Vec<String>>>,
        }
        impl Runner for Shared {
            fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
                Ok(self.lists.borrow_mut().remove(0))
            }
            fn spawn(&self, _: &TaskTarget, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn kill(&self, id: &str) -> anyhow::Result<()> {
                self.ops.borrow_mut().push(format!("kill:{id}"));
                Ok(())
            }
            fn remove(&self, id: &str) -> anyhow::Result<()> {
                self.ops.borrow_mut().push(format!("remove:{id}"));
                Ok(())
            }
        }
        let runner = Shared {
            lists: lists.clone(),
            ops: ops.clone(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = EvalCleanupGuard {
                runner,
                catalog: std::env::temp_dir().join("st2-test-panic"),
                host: "test".into(),
                keep: true,
            };
            panic!("panic after guard acquisition");
        }));
        assert!(result.is_err());
        assert_eq!(*lists.borrow(), Vec::<Vec<Session>>::new());
        assert_eq!(&*ops.borrow(), &["kill:panic", "remove:panic"]);
    }

    #[test]
    fn cleanup_guard_catalog_lifetime_matrix() {
        for keep in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let catalog = dir.path().join("catalog");
            std::fs::create_dir_all(&catalog).unwrap();
            let runner = RaceRunner {
                lists: RefCell::new(vec![
                    vec![Session {
                        pty_id: "x".into(),
                        alive: true,
                        exit_code: None,
                        presentation: None,
                    }],
                    vec![],
                ]),
                ops: RefCell::new(Vec::new()),
            };
            {
                let _guard = EvalCleanupGuard {
                    runner,
                    catalog: catalog.clone(),
                    host: "test".into(),
                    keep,
                };
            }
            assert_eq!(catalog.exists(), keep);
        }
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        let runner = PersistentRunner {
            lists: RefCell::new(0),
            ops: RefCell::new(0),
        };
        {
            let _guard = EvalCleanupGuard {
                runner,
                catalog: catalog.clone(),
                host: "test".into(),
                keep: false,
            };
        }
        assert!(catalog.exists());
    }

    #[test]
    fn signal_judges_do_not_gate_the_verdict() {
        let j = |name: &str, passed: bool, signal: bool| JudgeResult {
            name: name.into(),
            passed,
            detail: String::new(),
            signal,
        };
        let report = |judges: Vec<JudgeResult>| EvalReport {
            done: true,
            judges,
            timeout: Duration::ZERO,
        };
        // A FAILING signal judge does NOT gate a passing gating judge.
        assert!(report(vec![j("gate", true, false), j("sig", false, true)]).passed());
        // A failing GATING judge does gate.
        assert!(!report(vec![j("gate", false, false), j("sig", true, true)]).passed());
        // Only signal judges (nothing gating) → NOT a pass (asserts nothing).
        assert!(!report(vec![j("sig", true, true)]).passed());
    }

    #[test]
    fn compact_agents_use_one_host_qualified_identity_namespace() {
        let spec = parse_spec(
            r#"
            env { ST_ROOT "$CATALOG/custom-bus" }
            team "mix" {
              agent "sup" {
                workspace "./sup"
                command "exec claude 'boot'"
                ding
              }
              agent "worker" {
                workspace "./worker"
                supervisor "mix.sup"
                command "exec claude 'work'"
                ding
              }
            }
            "#,
        )
        .unwrap();
        let root = Path::new("/tmp/eval-root");
        let specs = spec_to_agent_specs(&spec.agents, "evalhost", root);
        assert_eq!(specs.len(), 2);
        let sup = &specs[0];
        let worker = &specs[1];
        assert_eq!(sup.identity, "mix.sup");
        assert_eq!(sup.host.as_deref(), Some("evalhost"));
        assert_eq!(sup.job_type, JobType::Service);
        assert_eq!(sup.workspace.as_deref(), Some("./sup"));
        assert_eq!(worker.supervisor.as_deref(), Some("evalhost.mix.sup"));
        // spec.path.parent() must be `root` (the cwd/$CATALOG base).
        assert_eq!(sup.path.parent().unwrap(), root);
        assert!(matches!(sup.tasks[0].kind, TaskKind::Pty));
        assert_eq!(sup.tasks[0].id.as_deref(), Some("evalhost.mix.sup"));
        assert_eq!(sup.tasks[0].command.as_deref(), Some("exec claude 'boot'"));
        assert_eq!(
            sup.tasks[0].env.get("ST_ROOT").unwrap(),
            "$CATALOG/custom-bus"
        );
        assert!(matches!(sup.tasks[1].kind, TaskKind::Exec));
        assert_eq!(sup.tasks[1].id.as_deref(), Some("evalhost.mix.sup.ding"));
        assert_eq!(
            sup.tasks[1].command.as_deref(),
            Some("st2 ding --identity evalhost.mix.sup --root $ST_ROOT")
        );

        let plan = reconcile(&specs, &[], "evalhost").unwrap();
        let targets = plan
            .launch
            .iter()
            .flat_map(|launch| launch.tasks.iter())
            .map(|target| (target.pty_id.as_str(), &target.env))
            .collect::<BTreeMap<_, _>>();
        for id in [
            "evalhost.mix.sup",
            "evalhost.mix.sup.ding",
            "evalhost.mix.worker",
            "evalhost.mix.worker.ding",
        ] {
            assert_eq!(
                targets[id].get("ST_AGENT").map(String::as_str),
                Some(id.trim_end_matches(".ding"))
            );
        }
        assert_eq!(
            targets["evalhost.mix.worker"]
                .get("ST_SUPERVISOR")
                .map(String::as_str),
            Some("evalhost.mix.sup")
        );
    }

    #[test]
    fn supervised_exit_markers_share_the_projected_runtime_identity() {
        let parsed = parse_spec(r#"agent "worker" { command "exit 0" }"#).unwrap();
        let catalog = tempfile::tempdir().unwrap();
        let mut specs = spec_to_agent_specs(&parsed.agents, "evalhost", catalog.path());

        add_eval_exit_markers(&mut specs, catalog.path(), "evalhost");

        let command = specs[0].tasks[0].command.as_deref().unwrap();
        let canonical = catalog.path().join(".eval-exits/evalhost.worker.status");
        let flat = catalog.path().join(".eval-exits/worker.status");
        assert!(command.contains(&canonical.display().to_string()));
        assert!(!command.contains(&flat.display().to_string()));

        std::fs::write(&canonical, "0\n").unwrap();
        std::fs::write(&flat, "1\n").unwrap();
        assert_eq!(eval_exit_code(catalog.path(), "evalhost.worker"), Some(0));
    }

    #[test]
    fn canonical_identity_conflict_refuses_before_render_changes_workspace() {
        let catalog = tempfile::tempdir().unwrap();
        let workspace = catalog.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(catalog.path().join("_templates")).unwrap();
        std::fs::write(workspace.join("existing"), "original\n").unwrap();
        std::fs::write(
            catalog.path().join("_templates/replacement"),
            "replacement\n",
        )
        .unwrap();
        write_eval_agent(
            catalog.path(),
            "agents/evalhost/worker/agent.kdl",
            r#"agent "worker" {
  identity "worker"
  host "evalhost"
  workspace "$CATALOG/workspace"
  env { ST_AGENT "wrong.actor" }
  argv "true"
  render {
    copy "_templates/replacement" "existing"
    copy "_templates/replacement" "new"
  }
}"#,
        );

        let error = load_canonical_eval_team(catalog.path(), "evalhost")
            .unwrap_err()
            .to_string();

        assert!(error.contains("conflicting ST_AGENT"), "{error}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("existing")).unwrap(),
            "original\n"
        );
        assert!(!workspace.join("new").exists());
    }

    #[test]
    fn task_compile_derived_ding_binds_exact_executable_and_ignores_task_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("catalog root");
        let exact = tmp.path().join("exact bin/st2 current");
        let trap = tmp.path().join("path trap");
        let exact_receipt = tmp.path().join("exact-receipt");
        let trap_receipt = tmp.path().join("trap-receipt");
        std::fs::create_dir_all(exact.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&trap).unwrap();
        std::fs::write(
            &exact,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                shell_single_quote(&exact_receipt.display().to_string())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exact, std::fs::Permissions::from_mode(0o755)).unwrap();
        let trap_st2 = trap.join("st2");
        std::fs::write(
            &trap_st2,
            format!(
                "#!/bin/sh\n: > {}\nexit 99\n",
                shell_single_quote(&trap_receipt.display().to_string())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&trap_st2, std::fs::Permissions::from_mode(0o755)).unwrap();

        let source = format!(
            r#"agent "worker" {{
  env {{ PATH "{}"; ST_ROOT "$CATALOG/bus root" }}
  command "st2 ding --identity authored-agent --root '/agent root'"
  ding
  exec "authored" {{ command "st2 ding --identity authored --root '/authored root'" }}
}}"#,
            trap.display()
        );
        let spec = parse_spec(&source).unwrap();
        let context = TaskCompileContext::new(root.clone(), exact.clone()).unwrap();
        let mut specs = spec_to_agent_specs(&spec.agents, "host", &root);
        compile_generated_ding_tasks(&mut specs, "host", &context).unwrap();
        assert_eq!(
            specs[0].tasks[0].command.as_deref(),
            Some("st2 ding --identity authored-agent --root '/agent root'")
        );
        assert_eq!(specs[0].tasks[0].argv, None);
        let derived = &specs[0].tasks[1];

        assert!(derived.derived);
        assert_eq!(derived.command, None);
        assert_eq!(
            derived.argv.as_deref(),
            Some(
                [
                    exact.display().to_string(),
                    "ding".into(),
                    "--identity".into(),
                    "host.worker".into(),
                    "--root".into(),
                    root.join("bus root").display().to_string(),
                ]
                .as_slice()
            )
        );
        assert_eq!(derived.env.get("PATH"), Some(&trap.display().to_string()));
        assert!(
            !derived
                .argv
                .as_ref()
                .unwrap()
                .iter()
                .any(|arg| arg == "sh" || arg == "-c")
        );
        let argv = derived.argv.as_ref().unwrap();
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", &trap)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&exact_receipt).unwrap(),
            format!(
                "ding\n--identity\nhost.worker\n--root\n{}\n",
                root.join("bus root").display()
            )
        );
        assert!(
            !trap_receipt.exists(),
            "the decoy PATH executable must never run"
        );

        let authored = &specs[0].tasks[2];
        assert!(!authored.derived);
        assert_eq!(
            authored.command.as_deref(),
            Some("st2 ding --identity authored --root '/authored root'")
        );
        assert_eq!(authored.argv, None);
    }

    #[test]
    fn task_compile_rejects_a_missing_captured_st2_despite_a_path_decoy() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing st2");
        let trap = tmp.path().join("path/st2");
        std::fs::create_dir_all(trap.parent().unwrap()).unwrap();
        std::fs::write(&trap, "decoy").unwrap();
        let error = TaskCompileContext::new(tmp.path().to_path_buf(), missing.clone()).unwrap_err();

        assert!(
            error.to_string().contains("does not exist")
                && error.to_string().contains(&missing.display().to_string()),
            "unexpected error for {}: {error:#}",
            missing.display()
        );
        assert!(trap.exists(), "the PATH decoy is a positive control");
    }

    #[test]
    fn task_compile_compact_and_eval_agents_share_exact_derived_ding_lowering() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("catalog");
        let agent_dir = root.join("agents/host/worker");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.kdl"),
            r#"agent "worker" {
  host "host"
  env { ST_ROOT "$CATALOG/bus root" }
  command "true"
  ding
}"#,
        )
        .unwrap();
        let exact = tmp.path().join("st2 exact");
        std::fs::write(&exact, "exact").unwrap();
        let context = TaskCompileContext::new(root.clone(), exact).unwrap();
        let mut compact_specs = agent_spec::discovery::discover(&root).specs;
        let eval = parse_spec(
            r#"eval {
  message { from "requester"; to "worker"; content "go" }
  max-timeout "5s"
  agent "worker" {
    env { ST_ROOT "$CATALOG/bus root" }
    command "true"
    ding
  }
}"#,
        )
        .unwrap();

        compile_generated_ding_tasks(&mut compact_specs, "host", &context).unwrap();
        let mut eval_specs = spec_to_agent_specs(&eval.eval.unwrap().agents, "host", &root);
        compile_generated_ding_tasks(&mut eval_specs, "host", &context).unwrap();
        let compact_ding = compact_specs[0]
            .tasks
            .iter()
            .find(|task| task.derived)
            .unwrap();
        let eval_ding = eval_specs[0]
            .tasks
            .iter()
            .find(|task| task.derived)
            .unwrap();
        let compact_runtime =
            task_runtime_id(&compact_specs[0], &compact_specs[0].tasks[0], "host");
        let eval_runtime = task_runtime_id(&eval_specs[0], &eval_specs[0].tasks[0], "host");

        assert_eq!(compact_ding.command, eval_ding.command);
        assert_eq!(compact_ding.argv, eval_ding.argv);
        assert_eq!(compact_runtime, "host.worker");
        assert_eq!(eval_runtime, "host.worker");
        assert_eq!(compact_ding.argv.as_ref().unwrap()[3], compact_runtime);
        assert_eq!(eval_ding.argv.as_ref().unwrap()[3], eval_runtime);
    }

    fn seed_msg(inbox: &Path, ts: u64, rand: &str, from: &str) {
        std::fs::create_dir_all(inbox).unwrap();
        std::fs::write(
            inbox.join(format!("{ts:013}-{rand}.md")),
            format!("---\nfrom: {from}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn copy_tree_renames_git_and_keeps_the_working_tree() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("worker/_git")).unwrap();
        std::fs::write(
            src.path().join("worker/_git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        std::fs::write(src.path().join("worker/LICENSE"), "proprietary\n").unwrap();
        copy_tree(src.path(), dst.path()).unwrap();
        assert!(
            dst.path().join("worker/.git/HEAD").is_file(),
            ".git materialized from _git"
        );
        assert!(
            !dst.path().join("worker/_git").exists(),
            "_git renamed away"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("worker/LICENSE")).unwrap(),
            "proprietary\n"
        );
    }

    #[test]
    fn resolve_content_reads_a_file_else_inline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("task.md"), "the task\n").unwrap();
        assert_eq!(
            resolve_content("./task.md", dir.path()).unwrap(),
            "the task\n"
        );
        assert_eq!(
            resolve_content("just do it inline", dir.path()).unwrap(),
            "just do it inline"
        );
    }

    #[test]
    fn wait_done_fires_only_on_a_confirmation_post_dating_a_worker_report() {
        let bus = tempfile::tempdir().unwrap();
        let root = bus.path();
        let (sup, req) = ("mix.sup", "requester");
        let workers = vec!["mix.worker".to_string()];
        let noop = &mut (|| {}) as &mut dyn FnMut();
        // No worker report → never fires (bounded timeout).
        assert!(!wait_done(
            root,
            None,
            sup,
            req,
            &workers,
            None,
            None,
            Duration::from_millis(100),
            noop,
        ));
        // A worker→sup report at t=2000 + a sup→requester confirm that PRE-dates it (t=1000) → false
        // (the early "on it" ack the discriminator exists to reject).
        seed_msg(
            &root.join(sup).join("inbox"),
            1_700_000_002_000,
            "aaaaaa",
            "mix.worker",
        );
        seed_msg(
            &root.join(req).join("inbox"),
            1_700_000_001_000,
            "bbbbbb",
            "mix.sup",
        );
        assert!(!wait_done(
            root,
            None,
            sup,
            req,
            &workers,
            None,
            None,
            Duration::from_millis(100),
            noop,
        ));
        // A confirm that POST-dates the report (t=3000) → done.
        seed_msg(
            &root.join(req).join("inbox"),
            1_700_000_003_000,
            "cccccc",
            "mix.sup",
        );
        assert!(wait_done(
            root,
            None,
            sup,
            req,
            &workers,
            None,
            None,
            Duration::from_millis(2000),
            noop,
        ));
    }

    #[test]
    fn wait_done_finds_a_worker_report_the_sup_already_archived() {
        // DING-BUS mandates archive-on-act, so a well-behaved sup MOVES the worker report inbox→archive
        // the instant it acts. The done-signal must still fire by scanning the archive — not hang to
        // max-timeout because the report left the inbox (the ghost-bug-codex flake evals-claude found).
        let bus = tempfile::tempdir().unwrap();
        let root = bus.path();
        let (sup, req) = ("mix.sup", "requester");
        let workers = vec!["mix.worker".to_string()];
        let noop = &mut (|| {}) as &mut dyn FnMut();
        // The report is ONLY in the sup's archive (inbox is empty — the sup archived on-act); the confirm
        // post-dates it in the requester's inbox → the loop closed → done must fire.
        seed_msg(
            &root.join(sup).join("archive"),
            1_700_000_002_000,
            "aaaaaa",
            "mix.worker",
        );
        seed_msg(
            &root.join(req).join("inbox"),
            1_700_000_003_000,
            "cccccc",
            "mix.sup",
        );
        assert!(
            wait_done(
                root,
                None,
                sup,
                req,
                &workers,
                None,
                None,
                Duration::from_millis(2000),
                noop,
            ),
            "an archived worker report must still be seen (else archive-on-act hygiene hangs the eval)"
        );
    }

    #[test]
    fn wait_done_runs_the_supervise_tick_while_waiting() {
        // Under `supervise`, wait_done calls its per-tick hook each poll — that hook is what respawns a
        // dead seat (via up_once_specs, proven separately). Here: no done signal → it times out, and the
        // tick must have run at least once during the wait.
        let bus = tempfile::tempdir().unwrap();
        let ticks = std::cell::Cell::new(0u32);
        let mut tick = || ticks.set(ticks.get() + 1);
        let fired = wait_done(
            bus.path(),
            None,
            "sup",
            "req",
            &["w".to_string()],
            None,
            None,
            Duration::from_millis(400),
            &mut tick,
        );
        assert!(!fired, "no confirmation was seeded → must time out");
        assert!(
            ticks.get() >= 1,
            "the supervise tick must run during the wait"
        );
    }

    #[test]
    fn canonical_singleton_done_is_new_after_kickoff_and_allows_the_same_millisecond() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agents/h/interviewer");
        let routes = BTreeMap::from([(
            "h.interviewer".to_string(),
            CanonicalRoute {
                inbox: crate::message::inbox_dir(&agent_dir),
                archive: crate::message::archive_dir(&agent_dir),
            },
        )]);
        let requester = root.path().join("requester/inbox");
        let kickoff_ts = 1_700_000_002_000;
        // An already-present message may even claim a FUTURE timestamp. Causality comes from the
        // pre-kickoff filename snapshot, not wall-clock trust.
        seed_msg(&requester, kickoff_ts + 1_000, "aaaaaa", "h.interviewer");
        let before = crate::message::list_dir(&requester)
            .unwrap()
            .into_iter()
            .map(|message| message.filename)
            .collect::<HashSet<_>>();
        let noop = &mut (|| {}) as &mut dyn FnMut();
        assert!(
            !wait_done(
                root.path(),
                Some(&routes),
                "h.interviewer",
                "requester",
                &[],
                Some(kickoff_ts),
                Some(&before),
                Duration::from_millis(100),
                noop,
            ),
            "a future-dated pre-kickoff acknowledgement must not complete a singleton"
        );
        // Filename novelty establishes after-kickoff causality; `>=` keeps a valid same-ms reply.
        seed_msg(&requester, kickoff_ts, "bbbbbb", "h.interviewer");
        assert!(
            wait_done(
                root.path(),
                Some(&routes),
                "h.interviewer",
                "requester",
                &[],
                Some(kickoff_ts),
                Some(&before),
                Duration::from_secs(1),
                noop,
            ),
            "a newly appearing same-ms singleton confirmation should complete promptly"
        );
    }

    #[test]
    fn canonical_boot_report_requires_every_admitted_task_and_propagates_backend_errors() {
        let ids = vec!["task-a".to_string(), "task-b".to_string()];
        let clean = UpReport {
            launched: ids.clone(),
            ..UpReport::default()
        };
        require_canonical_boot(&clean, &ids).unwrap();

        let missing = UpReport {
            launched: vec!["task-a".to_string()],
            ..UpReport::default()
        };
        assert!(
            require_canonical_boot(&missing, &ids)
                .unwrap_err()
                .to_string()
                .contains("task-b")
        );

        let backend_error = UpReport {
            launched: ids.clone(),
            errors: vec!["spawn task-b: backend refused".to_string()],
            ..UpReport::default()
        };
        assert!(
            require_canonical_boot(&backend_error, &ids)
                .unwrap_err()
                .to_string()
                .contains("backend refused")
        );
    }

    #[test]
    fn bus_root_expands_st_root_else_defaults() {
        let s = parse_spec("env { ST_ROOT \"$CATALOG/bus\" }\nagent \"a\" { command \"run\" }")
            .unwrap();
        assert_eq!(
            bus_root(&s, Path::new("/tmp/cat")),
            PathBuf::from("/tmp/cat/bus")
        );
        let s2 = parse_spec(r#"agent "a" { command "run" }"#).unwrap();
        assert_eq!(
            bus_root(&s2, Path::new("/tmp/cat")),
            PathBuf::from("/tmp/cat"),
            "a native eval with no authored ST_ROOT shares the task runtime's flat catalog bus"
        );
    }

    #[test]
    fn parse_pass_fail_takes_the_first_token() {
        assert_eq!(parse_pass_fail("PASS — cites the real commit"), Some(true));
        assert_eq!(parse_pass_fail("FAIL — vague 'done!'"), Some(false));
        assert_eq!(
            parse_pass_fail("PASS, though it could FAIL on edge cases"),
            Some(true)
        ); // PASS first
        assert_eq!(parse_pass_fail("verdict: fail, because…"), Some(false));
        assert_eq!(parse_pass_fail("no verdict here"), None);
    }

    #[test]
    fn declarative_checks_file_json() {
        let cat = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cat.path().join("worker")).unwrap();
        std::fs::write(
            cat.path().join("worker/LICENSE"),
            "Permission is hereby granted, free of charge\n",
        )
        .unwrap();
        std::fs::write(
            cat.path().join("worker/package.json"),
            r#"{"name":"w","license":"MIT","ok":true,"count":3}"#,
        )
        .unwrap();
        let ok = [
            Check::FileHas {
                path: "worker/LICENSE".into(),
                text: "Permission is hereby granted".into(),
            },
            Check::FileLacks {
                path: "worker/LICENSE".into(),
                text: "proprietary".into(),
            },
            Check::JsonField {
                path: "worker/package.json".into(),
                field: "license".into(),
                value: crate::eval_spec::JsonScalar::String("MIT".into()),
            },
        ];
        assert!(run_declarative(&ok, cat.path()).0);
        // A wrong json value fails.
        let bad = [Check::JsonField {
            path: "worker/package.json".into(),
            field: "license".into(),
            value: crate::eval_spec::JsonScalar::String("GPL".into()),
        }];
        assert!(!run_declarative(&bad, cat.path()).0);
        let typed = [
            Check::JsonField {
                path: "worker/package.json".into(),
                field: "ok".into(),
                value: crate::eval_spec::JsonScalar::Bool(true),
            },
            Check::JsonField {
                path: "worker/package.json".into(),
                field: "count".into(),
                value: crate::eval_spec::JsonScalar::Integer(3),
            },
        ];
        assert!(run_declarative(&typed, cat.path()).0);
        let mismatches = [
            Check::JsonField {
                path: "worker/package.json".into(),
                field: "count".into(),
                value: crate::eval_spec::JsonScalar::String("3".into()),
            },
            Check::JsonField {
                path: "worker/package.json".into(),
                field: "ok".into(),
                value: crate::eval_spec::JsonScalar::String("true".into()),
            },
        ];
        assert!(!run_declarative(&mismatches[..1], cat.path()).0);
        assert!(!run_declarative(&mismatches[1..], cat.path()).0);
        std::fs::write(cat.path().join("worker/malformed.json"), "{").unwrap();
        let malformed = [Check::JsonField {
            path: "worker/malformed.json".into(),
            field: "n".into(),
            value: crate::eval_spec::JsonScalar::Integer(1),
        }];
        let missing_json = [Check::JsonField {
            path: "worker/missing.json".into(),
            field: "n".into(),
            value: crate::eval_spec::JsonScalar::Integer(1),
        }];
        assert!(!run_declarative(&malformed, cat.path()).0);
        assert!(!run_declarative(&missing_json, cat.path()).0);
        // FileHas on a missing file fails.
        let missing = [Check::FileHas {
            path: "worker/NOPE".into(),
            text: "x".into(),
        }];
        assert!(!run_declarative(&missing, cat.path()).0);
    }

    #[test]
    fn run_steps_streams_large_step_output_to_files_without_corruption() {
        let catalog = tempfile::tempdir().unwrap();
        let cat = catalog.path();
        // ~64 KiB each of stdout and stderr — far beyond any pipe buffer. This round-trips byte
        // for byte only if the child streams straight into the capture files, and the combined log
        // keeps the stdout-then-stderr ordering.
        let marker_out = "O".repeat(64 * 1024);
        let marker_err = "E".repeat(64 * 1024);
        let step = RunStep {
            id: "big".into(),
            workspace: None,
            command: "printf '%s' \"$BIG_OUT\"; printf '%s' \"$BIG_ERR\" >&2".into(),
            env: BTreeMap::from([
                ("BIG_OUT".to_string(), marker_out.clone()),
                ("BIG_ERR".to_string(), marker_err.clone()),
            ]),
            unset: vec![],
            retry: None,
            allow_nonzero: false,
        };
        let (results, judge_env) = run_steps(&[step], cat, &BTreeMap::new());
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
        assert_eq!(judge_env.get("RUN_big_EXIT").map(String::as_str), Some("0"));
        let runs = cat.join(".runs");
        assert_eq!(
            std::fs::read(runs.join("big.out")).unwrap(),
            marker_out.as_bytes()
        );
        assert_eq!(
            std::fs::read(runs.join("big.err")).unwrap(),
            marker_err.as_bytes()
        );
        assert_eq!(std::fs::read_to_string(runs.join("big.exit")).unwrap(), "0");
        // Combined log = stdout fully followed by stderr.
        let mut expected = marker_out.into_bytes();
        expected.extend_from_slice(marker_err.as_bytes());
        assert_eq!(std::fs::read(cat.join("logs/big.log")).unwrap(), expected);
    }

    #[test]
    fn bash_judge_exit_code_and_timeout() {
        let spec = tempfile::tempdir().unwrap();
        let cat = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let (s, c, b) = (spec.path(), cat.path(), bus.path());
        let je = &BTreeMap::new();
        assert!(run_bash_judge("exit 0", s, c, b, Duration::from_secs(5), je).0);
        assert!(!run_bash_judge("exit 1", s, c, b, Duration::from_secs(5), je).0);
        // A grader that hangs past its timeout is killed → fail.
        assert!(!run_bash_judge("sleep 30", s, c, b, Duration::from_millis(300), je).0);
        // CWD is the SPEC folder (so ./judges/x.sh resolves); $CATALOG/$ST_ROOT reach the sandbox+bus.
        std::fs::create_dir_all(s.join("judges")).unwrap();
        std::fs::write(
            s.join("judges/ok.sh"),
            "#!/bin/sh\ntest -n \"$CATALOG\" && test -n \"$ST_ROOT\"\n",
        )
        .unwrap();
        assert!(
            run_bash_judge(
                "test \"$(pwd)\" = \"$SPEC_DIR\"",
                s,
                c,
                b,
                Duration::from_secs(5),
                je
            )
            .0
        );
        assert!(
            run_bash_judge("sh ./judges/ok.sh", s, c, b, Duration::from_secs(5), je).0,
            "./judges resolves from CWD=spec"
        );
    }

    #[test]
    fn resolve_spec_path_file_dir_and_non_spec() {
        let tmp = tempfile::tempdir().unwrap();
        // A bare .kdl file → spec.
        let f = tmp.path().join("thing.kdl");
        std::fs::write(&f, r#"agent "a" { command "run" }"#).unwrap();
        assert_eq!(resolve_spec_path(&f), Some(f.clone()));
        // A dir with one top-level parseable .kdl → that file.
        assert_eq!(resolve_spec_path(tmp.path()), Some(f.clone()));
        // A dir whose only .kdl does NOT parse as a spec → None (fall back to catalog).
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp2.path().join("bad.kdl"), "this is (not kdl").unwrap();
        assert_eq!(resolve_spec_path(tmp2.path()), None);
        // An empty dir → None.
        let tmp3 = tempfile::tempdir().unwrap();
        assert_eq!(resolve_spec_path(tmp3.path()), None);
    }
}
