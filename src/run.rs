//! Execution (M2/M3) — the side-effecting half that turns a reconcile plan into real pty operations,
//! plus the supervisor loop that reconciles on a folder-watch + timer.
//!
//! Everything st2 does to the world goes through the [`Runner`] trait: list sessions, spawn a pty
//! from its explicit launch, kill a session, remove a dead one. The production [`PtyCli`] shells out
//! to the `pty` CLI; tests swap in a fake, so plan execution is verified without spawning a single
//! real process. st2 stays harness-agnostic here too — it either runs shell source verbatim under
//! `sh -c` or passes a structured argv directly.
//!
//! The loop is decoupled Nomad-style: stopping st2 never tears down its agents — they are detached
//! pty sessions and keep running; only a `retired` spec tears an agent down.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read as _, Seek as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::exec_backend::ExecBackend;
use crate::flapping::FlappingCap;
use crate::message;
use crate::reconcile::{ReconcilePlan, Session, TaskLaunch, TaskTarget};
use agent_spec::spec::TaskKind;

// This is an outer containment bound for a wedged runtime, not a fleet-scalability mechanism.
const PTY_LIST_TIMEOUT: Duration = Duration::from_secs(2);
const PTY_DAEMON_SHUTDOWN_WAIT: Duration = Duration::from_secs(6);

/// Run a non-interactive child with bounded output capture. Regular temporary files keep an escaped
/// descendant that inherited stdout/stderr from blocking cleanup after the direct child times out.
/// The child still gets a fresh process group so the common wrapper-and-descendants case is reaped.
fn output_with_timeout(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
            let _ = child.kill();
            // Never turn the timeout into another unbounded wait. Reap asynchronously so a
            // long-running supervisor does not accumulate zombies, while a wedged runtime cannot
            // hold up this failed probe or a short-lived doctor process.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            anyhow::bail!("timed out after {:.1}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes)?;
    stderr.read_to_end(&mut stderr_bytes)?;
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

/// Resolve a task's working directory: declared `cwd` (expanded), else the agent's `workspace`
/// (expanded), else the spec file's directory (spec.md §2). A relative value is joined to the spec
/// dir; an absolute one replaces it.
pub(crate) fn resolve_task_cwd(
    target: &TaskTarget,
    spec_dir: &Path,
    catalog_root: &Path,
) -> PathBuf {
    match target.cwd.as_deref().or(target.workspace.as_deref()) {
        Some(c) => spec_dir.join(crate::expand::expand_catalog(c, catalog_root)),
        None => spec_dir.to_path_buf(),
    }
}

/// The set of task operations st2 needs. Abstracted so execution is testable against a fake.
pub trait Runner {
    /// ACTUAL state: every task session the runner can see (unioned across backends).
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>>;
    /// Spawn `target` in the background from its explicit launch. `spec_dir` is the spec file's
    /// directory — part of the cwd fallback chain (task.cwd → workspace → spec dir).
    fn spawn(&self, target: &TaskTarget, spec_dir: &Path) -> anyhow::Result<()>;
    /// SIGTERM a running session.
    fn kill(&self, pty_id: &str) -> anyhow::Result<()>;
    /// Reap an exited session before restarting it. Backends may preserve bounded diagnostics here.
    fn reap_for_restart(&self, pty_id: &str) -> anyhow::Result<()> {
        self.remove(pty_id)
    }
    /// Finally remove an exited session's files (retirement/final garbage collection).
    fn remove(&self, pty_id: &str) -> anyhow::Result<()>;
}

/// Production [`Runner`]. Shells out to the `pty` CLI for tasks. (M1a routes both `pty` and `exec`
/// tasks here; the terminal-free `exec` backend lands in M1b — R09.)
pub struct PtyCli {
    /// The `pty` binary (defaults to `pty` on PATH).
    bin: String,
    /// The catalog root — the value of `$CATALOG` during `$`-expansion (spec.md §2 / R11).
    catalog_root: PathBuf,
}

impl Default for PtyCli {
    fn default() -> Self {
        Self {
            bin: "pty".to_string(),
            catalog_root: PathBuf::from("."),
        }
    }
}

/// One entry of `pty list --json` — only the fields st2 needs.
#[derive(Debug, Deserialize)]
struct PtyListEntry {
    /// The pinned session id (matches `--id`), st2's key back to a declared task.
    name: String,
    /// `running` | `exited` | `vanished`.
    status: String,
    /// The process exit code once `exited` (absent while running or `vanished`).
    #[serde(rename = "exitCode", default)]
    exit_code: Option<i64>,
}

/// The `PTY_ROOT` st2 uses for a pty op. An EXPORTED ambient `PTY_ROOT` WINS — a decoupled partition,
/// e.g. an eval run's short `/tmp/stev-<runid>` that dodges the 104-byte unix-socket-path limit that a
/// deep `<catalog>/pty` would blow — else what the catalog itself declares
/// ([`crate::catalog::pty_root`]), else the native default `<catalog>/pty`. Applied uniformly to
/// spawn and list/kill so st2 always manages sessions where it put them.
pub fn effective_pty_root(catalog_root: &Path) -> PathBuf {
    effective_pty_root_from(catalog_root, std::env::var_os("PTY_ROOT"))
}

/// The testable core of [`effective_pty_root`] — the ambient value is injected rather than read from
/// the process env, so tests don't race on the global environment.
fn effective_pty_root_from(catalog_root: &Path, ambient: Option<std::ffi::OsString>) -> PathBuf {
    match ambient {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => crate::catalog::pty_root(catalog_root),
    }
}

impl PtyCli {
    /// A `PtyCli` rooted at `catalog_root` (used for `$CATALOG` expansion).
    pub fn new(catalog_root: PathBuf) -> Self {
        Self {
            bin: "pty".to_string(),
            catalog_root,
        }
    }

    /// Expand `$VAR`/`${VAR}` against the ambient env plus `$CATALOG` = the catalog root.
    fn expand(&self, s: &str) -> String {
        crate::expand::expand_catalog(s, &self.catalog_root)
    }

    /// Resolve a task's working directory (see [`resolve_task_cwd`]).
    fn resolve_cwd(&self, target: &TaskTarget, spec_dir: &Path) -> PathBuf {
        resolve_task_cwd(target, spec_dir, &self.catalog_root)
    }

    /// The st2-owned part of a PTY task's environment. This same final map is both inherited by the
    /// initial `pty run` process and persisted through repeatable `--env KEY=VALUE` arguments, so a
    /// manual `pty restart` recreates the task without snapshotting unrelated ambient OS variables.
    fn managed_task_env(&self, target: &TaskTarget) -> BTreeMap<OsString, OsString> {
        let mut env = BTreeMap::from([
            (
                OsString::from("CATALOG"),
                self.catalog_root.as_os_str().to_os_string(),
            ),
            (
                OsString::from("ST_ROOT"),
                self.catalog_root.as_os_str().to_os_string(),
            ),
            (
                OsString::from("PTY_ROOT"),
                effective_pty_root(&self.catalog_root).into_os_string(),
            ),
            (OsString::from("TERM"), OsString::from("xterm-256color")),
        ]);
        if let Ok(path) = crate::hooks::hooks_root() {
            env.insert(OsString::from("ST_HOOKS"), path.into_os_string());
        }
        for (key, value) in &target.env {
            let value = if key == "PTY_ROOT" {
                effective_pty_root(&self.catalog_root).into_os_string()
            } else {
                OsString::from(self.expand(value))
            };
            env.insert(OsString::from(key), value);
        }
        env
    }

    /// Build (but do not run) the `pty run` invocation for `target`. Split out so the exact argv +
    /// env can be unit-tested without spawning anything.
    ///
    /// `$VAR`s are expanded here for everything that does NOT pass through a shell — env values, tag
    /// values, `cwd`, and direct argv — because `pty` passes them through verbatim. Shell source is
    /// left unexpanded: `sh -c` expands it at spawn from the same env (which includes `$CATALOG`).
    fn build_run_command(&self, target: &TaskTarget, spec_dir: &Path) -> Command {
        let cwd = self.resolve_cwd(target, spec_dir);
        let mut cmd = Command::new(&self.bin);
        cmd.arg("run")
            .arg("-d") // detached: leave it running in the background
            .arg("--force") // st2 itself may run inside a pty session; allow nesting
            .args(["--id", &target.pty_id]);
        // Keep the adoption key task-specific, but make a differing human-facing label the owning
        // agent's stable bus identity instead of pty's auto-derived `<cwd>-sh` label. When the
        // lifecycle id already IS that identity, suppress pty's automatic `<cwd>-sh` alias: pty
        // rejects displayName == id, and no displayName makes the UI fall back to the stable id.
        if target.pty_id == target.bus_id {
            cmd.arg("--no-display-name");
        } else {
            cmd.args(["--name", &target.bus_id]);
        }
        cmd.arg("--cwd").arg(&cwd);
        for (k, v) in &target.tags {
            cmd.arg("--tag").arg(format!("{k}={}", self.expand(v)));
        }
        // Managed agent and DING sessions retain PTY exit evidence until the lifecycle owner records
        // the receipt and explicitly removes the generation. This prevents face607 clean-exit reaping
        // from erasing diagnostics needed for adoption/loss investigation.
        if target.name == "agent" || target.name == "ding" {
            cmd.arg("--tag").arg("keep=true");
        }
        // Apply the resolved managed overlay to the initial launcher exactly as before, and also
        // persist it in PTY metadata for manual restart. PTY applies repeated `--env` entries
        // last-wins, then forcibly injects the new session's own PTY_SESSION identity.
        let managed_env = self.managed_task_env(target);
        cmd.envs(&managed_env);
        for (key, value) in &managed_env {
            let mut assignment = key.clone();
            assignment.push("=");
            assignment.push(value);
            cmd.arg("--env").arg(assignment);
        }
        cmd.arg("--");
        match &target.launch {
            // Run shell source verbatim — st2 never parses or splits it.
            TaskLaunch::Shell(command) => {
                cmd.arg("sh").arg("-c").arg(command);
            }
            // Direct mode preserves argument boundaries and introduces no shell process.
            TaskLaunch::Argv(argv) => {
                debug_assert!(!argv.is_empty());
                cmd.args(argv.iter().map(|arg| self.expand(arg)));
            }
        }
        cmd
    }
}

impl Runner for PtyCli {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let out = output_with_timeout(
            Command::new(&self.bin)
                .args(["list", "--json"])
                .env("PTY_ROOT", effective_pty_root(&self.catalog_root)),
            PTY_LIST_TIMEOUT,
        )
        .map_err(|error| anyhow::anyhow!("`pty list --json` failed: {error}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty list --json` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let entries: Vec<PtyListEntry> = serde_json::from_slice(&out.stdout)
            .map_err(|e| anyhow::anyhow!("parsing `pty list --json`: {e}"))?;
        Ok(entries
            .into_iter()
            .map(|e| Session {
                pty_id: e.name,
                alive: e.status == "running",
                exit_code: e.exit_code,
            })
            .collect())
    }

    fn spawn(&self, target: &TaskTarget, spec_dir: &Path) -> anyhow::Result<()> {
        // Isolate the pty session in its own scope (R21b): `systemd-run --scope` wraps the `pty run`
        // invocation, and because the per-session `pty-daemon` inherits the caller's cgroup (a
        // double-fork does not move cgroups), the daemon + session land in the scope — a sibling of
        // the transport unit that a transport/supervisor cgroup-cascade cannot reach. Pass-through on
        // non-systemd hosts. The inner argv (built + unit-tested by `build_run_command`) is wrapped
        // verbatim and its env re-applied so it reaches `pty` (and thus the session) through the scope.
        let inner = self.build_run_command(target, spec_dir);
        let program = inner.get_program().to_os_string();
        let args: Vec<OsString> = inner.get_args().map(|a| a.to_os_string()).collect();
        let arg_refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        let unit = crate::isolate::scope_unit(&target.pty_id);
        let envs: Vec<(OsString, OsString)> = inner
            .get_envs()
            .filter_map(|(k, v)| v.map(|v| (k.to_os_string(), v.to_os_string())))
            .collect();

        // Atomic reap-then-respawn: a session id JUST reaped in this same pass (execute's
        // reap-then-respawn after a hard-kill) can linger microseconds in the per-session pty daemon —
        // `pty rm` frees the registry entry but the daemon's socket/lock isn't released yet, so an
        // immediate `pty run` fails "id already in use". Reap the lingering corpse + brief backoff +
        // retry closes the window WITHIN the pass, instead of leaving `--once` to error and relying on a
        // later loop cycle to self-heal (loop mode did; `--once` had one shot). Bounded; on a persistent
        // failure it surfaces the error unchanged.
        const SPAWN_ATTEMPTS: u32 = 4;
        let mut last_err = String::new();
        for attempt in 0..SPAWN_ATTEMPTS {
            let mut cmd = crate::isolate::wrap(&unit, program.as_os_str(), &arg_refs);
            for (k, v) in &envs {
                cmd.env(k, v);
            }
            let out = cmd.output()?;
            if out.status.success() {
                return Ok(());
            }
            last_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let corpse_race = last_err.contains("already in use");
            if !corpse_race || attempt + 1 == SPAWN_ATTEMPTS {
                break;
            }
            // A lingering corpse blocks the id — reap it, then back off before retrying.
            let _ = Command::new(&self.bin)
                .arg("rm")
                .arg(&target.pty_id)
                .env("PTY_ROOT", effective_pty_root(&self.catalog_root))
                .output();
            std::thread::sleep(Duration::from_millis(100 * u64::from(attempt + 1)));
        }
        anyhow::bail!("spawning pty '{}' failed: {last_err}", target.pty_id);
    }

    fn kill(&self, pty_id: &str) -> anyhow::Result<()> {
        let out = Command::new(&self.bin)
            .arg("kill")
            .arg(pty_id)
            .env("PTY_ROOT", effective_pty_root(&self.catalog_root))
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty kill {pty_id}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn reap_for_restart(&self, pty_id: &str) -> anyhow::Result<()> {
        // An exited PTY can remain in its daemon's 500ms shutdown window. Removing its files during
        // that window and immediately reusing the id is unsafe: the old generation's final cleanup
        // can unlink the new generation's socket/pid and leave `pty run` waiting for its full startup
        // timeout. Wait for the recorded daemon to finish its bounded shutdown before removing the
        // corpse. PTY may self-reap the files while we wait; "not found" is therefore success here.
        let pty_root = effective_pty_root(&self.catalog_root);
        let daemon_pid = std::fs::read_to_string(pty_root.join(format!("{pty_id}.pid")))
            .ok()
            .and_then(|raw| raw.trim().parse::<i32>().ok());
        if let Some(pid) = daemon_pid {
            let deadline = Instant::now() + PTY_DAEMON_SHUTDOWN_WAIT;
            while crate::host_lock::process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            if crate::host_lock::process_alive(pid) {
                anyhow::bail!(
                    "pty daemon {pid} for '{pty_id}' did not finish its bounded shutdown"
                );
            }
        }

        let out = Command::new(&self.bin)
            .arg("rm")
            .arg(pty_id)
            .env("PTY_ROOT", &pty_root)
            .output()?;
        if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not found") {
            return Ok(());
        }
        anyhow::bail!(
            "`pty rm {pty_id}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }

    fn remove(&self, pty_id: &str) -> anyhow::Result<()> {
        let out = Command::new(&self.bin)
            .arg("rm")
            .arg(pty_id)
            .env("PTY_ROOT", effective_pty_root(&self.catalog_root))
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty rm {pty_id}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// The production [`Runner`]: routes `pty` tasks to the `pty` CLI and `exec` tasks to the
/// terminal-free [`ExecBackend`], presenting one unified session view. kill/remove route by the kind
/// recorded during the last `list_sessions` (with a both-backends fallback).
pub struct SystemRunner {
    pty: PtyCli,
    exec: ExecBackend,
    /// id → kind, refreshed each `list_sessions`, so kill/remove hit the right backend.
    index: RefCell<HashMap<String, TaskKind>>,
}

impl SystemRunner {
    /// `catalog_root` roots `$CATALOG`; `exec_state_dir` is where exec pids/logs live (machine-local).
    pub fn new(catalog_root: PathBuf, exec_state_dir: PathBuf) -> Self {
        Self {
            pty: PtyCli::new(catalog_root.clone()),
            exec: ExecBackend::new(exec_state_dir, catalog_root),
            index: RefCell::new(HashMap::new()),
        }
    }
}

impl Runner for SystemRunner {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        let mut idx = self.index.borrow_mut();
        idx.clear();
        let mut all = self.pty.list_sessions()?;
        for s in &all {
            idx.insert(s.pty_id.clone(), TaskKind::Pty);
        }
        let ex = self.exec.list()?;
        for s in &ex {
            idx.insert(s.pty_id.clone(), TaskKind::Exec);
        }
        all.extend(ex);
        Ok(all)
    }

    fn spawn(&self, target: &TaskTarget, spec_dir: &Path) -> anyhow::Result<()> {
        match target.kind {
            TaskKind::Pty => self.pty.spawn(target, spec_dir),
            TaskKind::Exec => self.exec.spawn(target, spec_dir),
        }
    }

    fn kill(&self, pty_id: &str) -> anyhow::Result<()> {
        match self.index.borrow().get(pty_id) {
            Some(TaskKind::Exec) => self.exec.kill(pty_id),
            Some(TaskKind::Pty) => self.pty.kill(pty_id),
            None => self.pty.kill(pty_id).or_else(|_| self.exec.kill(pty_id)),
        }
    }

    fn reap_for_restart(&self, pty_id: &str) -> anyhow::Result<()> {
        match self.index.borrow().get(pty_id) {
            Some(TaskKind::Exec) => self.exec.reap_for_restart(pty_id),
            Some(TaskKind::Pty) => self.pty.reap_for_restart(pty_id),
            None => {
                let _ = self.pty.reap_for_restart(pty_id);
                self.exec.reap_for_restart(pty_id)
            }
        }
    }

    fn remove(&self, pty_id: &str) -> anyhow::Result<()> {
        match self.index.borrow().get(pty_id) {
            Some(TaskKind::Exec) => self.exec.remove(pty_id),
            Some(TaskKind::Pty) => self.pty.remove(pty_id),
            None => {
                let _ = self.pty.remove(pty_id);
                let _ = self.exec.remove(pty_id);
                Ok(())
            }
        }
    }
}

/// The machine-local runner-state dir for a host's exec tasks: `$XDG_STATE_HOME/st2/<host>/exec`
/// (falling back to `~/.local/state`). Not synced — pids are host-local.
pub fn exec_state_dir(host: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("st2").join(host).join("exec")
}

/// A task st2 gave up restarting (crash-looped past its `restart{}` policy, mode=fail) — carries what
/// the supervisor loop needs to SURFACE it: the parked task, its agent, and who to notify (M2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashLoop {
    /// The parked task's pty id.
    pub pty_id: String,
    /// The owning agent's identity and (declared) host — resolved to a bus id when surfacing.
    pub identity: String,
    pub host: Option<String>,
    /// The agent's `supervisor` (from its spec), the crash-ding recipient. `None` → nobody to notify.
    pub supervisor: Option<String>,
}

impl CrashLoop {
    /// The parked agent's bus id (`<host>.<identity>`), using `this_host` when the spec omits a host.
    pub fn agent_bus_id(&self, this_host: &str) -> String {
        format!(
            "{}.{}",
            self.host.as_deref().unwrap_or(this_host),
            self.identity
        )
    }
}

/// Owned, human-readable summary of one reconcile+execute pass (no borrows of the plan/specs).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UpReport {
    /// The pass could not obtain an authoritative session snapshot, so it deliberately performed no
    /// reconciliation. Long-running supervisors retry; a one-shot caller must exit unsuccessfully.
    pub skipped: bool,
    /// pty ids spawned this pass.
    pub launched: Vec<String>,
    /// pty ids torn down (retired agents) this pass.
    pub torn_down: Vec<String>,
    /// pty ids garbage-collected (dead, non-`keep`) this pass.
    pub gc: Vec<String>,
    /// pty ids whose GC/relaunch was DEFERRED this pass by the liveness debounce — a task that read
    /// not-alive but was alive within the grace window, i.e. a transient `pty list` flicker under load,
    /// left alone rather than destructively reaped (R21c). Not "noteworthy" (it's a no-op by design).
    pub deferred: Vec<String>,
    /// pty ids the flapping-cap refused to (re)launch this pass (parked / crash-looping).
    pub flapping: Vec<String>,
    /// Rich crash-loop records (a superset of `flapping`) — the source for supervisor surfacing.
    pub crash_loops: Vec<CrashLoop>,
    /// identities adopted (already fully present).
    pub adopted: Vec<String>,
    /// identities skipped as belonging to another host.
    pub other_host: Vec<String>,
    /// identities with no runnable task (unrendered).
    pub unrunnable: Vec<String>,
    /// discovery warnings (mismatches, …).
    pub warnings: Vec<String>,
    /// discovery + execution errors (non-fatal; collected).
    pub errors: Vec<String>,
}

impl UpReport {
    fn absorb(&mut self, mut other: UpReport) {
        self.skipped |= other.skipped;
        self.launched.append(&mut other.launched);
        self.torn_down.append(&mut other.torn_down);
        self.gc.append(&mut other.gc);
        self.deferred.append(&mut other.deferred);
        self.flapping.append(&mut other.flapping);
        self.crash_loops.append(&mut other.crash_loops);
        self.adopted.append(&mut other.adopted);
        self.other_host.append(&mut other.other_host);
        self.unrunnable.append(&mut other.unrunnable);
        self.warnings.append(&mut other.warnings);
        self.errors.append(&mut other.errors);
    }
    /// True when the pass actually changed something (or hit an error) — used to keep the loop's log
    /// quiet on no-op ticks.
    pub fn is_noteworthy(&self) -> bool {
        self.skipped
            || !self.launched.is_empty()
            || !self.torn_down.is_empty()
            || !self.gc.is_empty()
            || !self.flapping.is_empty()
            || !self.errors.is_empty()
    }
}

/// Execute a plan against a runner, folding results into `report` and consulting/updating the
/// flapping-cap. Order matters: reap standalone corpses, then (cap-gated) reap-and-respawn each
/// launch target, then kill teardowns. Per-op errors are collected, never fatal.
pub fn execute(
    plan: &ReconcilePlan,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    report: &mut UpReport,
) {
    // The corpses tied to a launch target (dead, non-keep, active ptys) are reaped inside the launch
    // loop so a parked flapper keeps its evidence. Everything else in `gc` (e.g. a retired agent's
    // dead sessions) is reaped here.
    let launch_ids: HashSet<&str> = plan
        .launch
        .iter()
        .flat_map(|l| l.tasks.iter().map(|t| t.pty_id.as_str()))
        .collect();
    let gc_set: HashSet<&str> = plan.gc.iter().map(String::as_str).collect();

    for id in &plan.gc {
        if launch_ids.contains(id.as_str()) {
            continue; // reaped in the launch loop, gated by the cap
        }
        match runner.remove(id) {
            Ok(()) => report.gc.push(id.clone()),
            Err(e) => report.errors.push(format!("rm {id}: {e}")),
        }
    }

    for launch in &plan.launch {
        let spec_dir = launch.spec.path.parent().unwrap_or_else(|| Path::new("."));
        let policy = launch.spec.restart_policy();
        for target in &launch.tasks {
            let now = Instant::now();
            match cap.decide(&target.pty_id, now, &policy) {
                crate::flapping::RestartDecision::Allow => {}
                crate::flapping::RestartDecision::GaveUp => {
                    // Parked (mode=fail exhausted): surface it, leave the corpse as evidence.
                    if !report.flapping.contains(&target.pty_id) {
                        report.flapping.push(target.pty_id.clone());
                        report.crash_loops.push(CrashLoop {
                            pty_id: target.pty_id.clone(),
                            identity: launch.spec.identity.clone(),
                            host: launch.spec.host.clone(),
                            supervisor: launch.spec.supervisor.clone(),
                        });
                    }
                    continue;
                }
                // Delaying / RateLimited: transient — skip quietly, retry a later pass, keep the corpse.
                crate::flapping::RestartDecision::Delaying
                | crate::flapping::RestartDecision::RateLimited => continue,
            }
            // Reap the corpse first (a dead session blocks respawn), preserving any backend-owned
            // bounded diagnostics, then respawn.
            if gc_set.contains(target.pty_id.as_str()) {
                match runner.reap_for_restart(&target.pty_id) {
                    Ok(()) => report.gc.push(target.pty_id.clone()),
                    Err(e) => {
                        report
                            .errors
                            .push(format!("reap {} for restart: {e}", target.pty_id));
                        continue;
                    }
                }
            }
            match runner.spawn(target, spec_dir) {
                Ok(()) => {
                    cap.record(&target.pty_id, now);
                    report.launched.push(target.pty_id.clone());
                }
                Err(e) => report.errors.push(format!("spawn {}: {e}", target.pty_id)),
            }
        }
    }

    for td in &plan.teardown {
        for id in &td.pty_ids {
            match runner.kill(id) {
                Ok(()) => report.torn_down.push(id.clone()),
                Err(e) => report.errors.push(format!("kill {id}: {e}")),
            }
        }
    }

    report
        .adopted
        .extend(plan.adopt.iter().map(|s| s.identity.clone()));
    report
        .other_host
        .extend(plan.other_host.iter().map(|s| s.identity.clone()));
    report
        .unrunnable
        .extend(plan.unrunnable.iter().map(|s| s.identity.clone()));
}

/// The grace window for the liveness debounce (see [`LivenessDebounce`]): a task read not-alive but
/// alive within this window is treated as a transient `pty list` flicker and left alone, not reaped.
/// ~10s comfortably covers a load-induced misread burst while a genuinely-dead task is still reaped
/// only one grace-window late.
const DEBOUNCE_GRACE: Duration = Duration::from_secs(10);

/// Absorbs transient `pty list` misreports so the reconcile loop never destructively GCs a HEALTHY
/// agent (R21c). Under concurrent multi-agent pty load (e.g. an eval run), `pty list --json` can
/// momentarily report a live session as not-running; reconcile would then classify it `Dead` → `pty
/// rm` (destroys the session) + re-launch. This tracks the last time each id was seen ALIVE; a task
/// that reads not-alive but was alive within [`DEBOUNCE_GRACE`] is a flicker — its GC and re-launch
/// are DEFERRED (left alone) until it reads not-alive continuously past the grace (a stable death,
/// which is then reaped normally). It defers BOTH the destructive GC and the noisy "already in use"
/// re-launch. A never-seen task (a genuinely-new launch) is never deferred.
pub struct LivenessDebounce {
    last_alive: HashMap<String, Instant>,
    grace: Duration,
}

impl LivenessDebounce {
    pub fn new(grace: Duration) -> Self {
        Self {
            last_alive: HashMap::new(),
            grace,
        }
    }

    /// Record which ids are alive as of `now`, and forget ids not seen alive within the grace (bounds
    /// memory; a long-dead id past the grace is no longer debounced anyway).
    fn observe(&mut self, sessions: &[Session], now: Instant) {
        for s in sessions {
            if s.alive {
                self.last_alive.insert(s.pty_id.clone(), now);
            }
        }
        self.last_alive
            .retain(|_, &mut t| now.duration_since(t) < self.grace);
    }

    /// True if `id` was seen alive within the grace ending at `now` — a recent flicker, defer it.
    fn recently_alive(&self, id: &str, now: Instant) -> bool {
        self.last_alive
            .get(id)
            .is_some_and(|&t| now.duration_since(t) < self.grace)
    }

    /// Remove recently-alive ids from the plan's GC and launch sets (they're flickers, not real
    /// deaths). Returns the deferred ids for the report. Genuinely-dead (past-grace) and never-seen
    /// (new) tasks are left in the plan and handled normally.
    fn defer_flickers(&self, plan: &mut ReconcilePlan, now: Instant) -> Vec<String> {
        let mut deferred = Vec::new();
        plan.gc.retain(|id| {
            let flicker = self.recently_alive(id, now);
            if flicker {
                deferred.push(id.clone());
            }
            !flicker
        });
        for launch in &mut plan.launch {
            launch.tasks.retain(|t| {
                let flicker = self.recently_alive(&t.pty_id, now);
                if flicker && !deferred.contains(&t.pty_id) {
                    deferred.push(t.pty_id.clone());
                }
                !flicker
            });
        }
        plan.launch.retain(|l| !l.tasks.is_empty());
        deferred
    }
}

/// One full reconcile pass: discover → list actual → reconcile → execute. On a `pty list` failure the
/// pass is SKIPPED (the error is recorded but nothing is reconciled) — treating a transient list
/// failure as "no sessions" would double-spawn everything. `cap` carries flapping state across passes;
/// `debounce` carries per-id liveness so a transient not-alive flicker isn't destructively reaped.
fn reconcile_pass(
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
) -> UpReport {
    let found = crate::discover(root);
    let mut report = UpReport {
        warnings: found.warnings.clone(),
        errors: found
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.path.display(), e.message))
            .collect(),
        ..Default::default()
    };

    // Verify before touching any Codex workspace. A missing/stale/partial hook set must not rewrite
    // an already-live agent's settings to a nonexistent path. Codex specs remain in reconciliation
    // so live sessions can still be adopted; only their materialization and any new launch defer.
    let hook_error = crate::hooks::required_by_codex(&found.specs, this_host, root)
        .then(crate::hooks::verify_required_set)
        .transpose()
        .err()
        .map(|error| error.to_string());
    if let Some(error) = &hook_error {
        report.errors.push(format!(
            "verify this binary's lifecycle hooks before Codex materialization: {error}; materialization deferred"
        ));
    }
    let materializable_specs = found
        .specs
        .iter()
        .filter(|spec| {
            hook_error.is_none() || !crate::hooks::required_by_codex_agent(spec, this_host, root)
        })
        .cloned()
        .collect::<Vec<_>>();

    // Ordered, idempotent pre-boot materialization. A gating render failure removes only that agent
    // from this pass; advisory git-exclude failures remain warnings and never block a launch.
    let materialized =
        crate::materialize::materialize_catalog(root, &materializable_specs, this_host);
    report.warnings.extend(materialized.warnings);
    report.errors.extend(materialized.errors);
    let eligible_specs: Vec<_> = found
        .specs
        .iter()
        .filter(|spec| !materialized.failed_agents.contains(&spec.bus_id(this_host)))
        .cloned()
        .collect();

    let sessions = match runner.list_sessions() {
        Ok(s) => s,
        Err(e) => {
            report.skipped = true;
            report
                .errors
                .push(format!("list sessions (pass skipped): {e}"));
            return report;
        }
    };
    let now = Instant::now();
    debounce.observe(&sessions, now);
    let mut plan = crate::reconcile(&eligible_specs, &sessions, this_host);
    report.deferred = debounce.defer_flickers(&mut plan, now);
    gate_codex_launches_on_hooks(&mut plan, root, &mut report, || match &hook_error {
        Some(error) => anyhow::bail!("{error}"),
        None => Ok(()),
    });
    execute(&plan, runner, cap, &mut report);
    report
}

/// A missing Codex agent must not launch against stale lifecycle hooks. Suppress the affected agent
/// launches (including their sidecars) and surface the error when hook verification fails.
///
/// Workspace trust belongs to the declared provider command and its selected account-specific
/// runtime. Reconciliation deliberately does not mutate an ambient Codex config: an account selector
/// may choose `CODEX_HOME` only after this process launches the command, so such a write would target
/// the wrong state and could not satisfy the launched seat's trust gate.
fn gate_codex_launches_on_hooks<'a, V>(
    plan: &mut ReconcilePlan<'a>,
    catalog_root: &Path,
    report: &mut UpReport,
    verify_hooks: V,
) where
    V: FnOnce() -> anyhow::Result<()>,
{
    let mut gated_agents = Vec::new();
    for launch in &plan.launch {
        let Some(_) = launch.tasks.iter().find(|target| {
            target.name == "agent"
                && crate::hooks::launch_invokes_codex(&target.launch, catalog_root)
        }) else {
            continue;
        };
        gated_agents.push(launch.spec.identity.clone());
    }
    if gated_agents.is_empty() {
        return;
    }

    if let Err(error) = verify_hooks() {
        plan.launch
            .retain(|launch| !gated_agents.contains(&launch.spec.identity));
        report.errors.push(format!(
            "verify lifecycle hooks for new Codex agent(s) {}: {error}; launch suppressed",
            gated_agents.join(", ")
        ));
    }
}

/// One reconcile pass with a throwaway flapping-cap (`st2 up --once`). Returns an owned report;
/// never `Err` — all failures are collected in `report.errors`. The debounce is throwaway too: a
/// single pass has no prior liveness history, so it defers nothing (correct — one-shot has no flicker).
pub fn up_once(root: &Path, this_host: &str, runner: &dyn Runner) -> anyhow::Result<UpReport> {
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    Ok(reconcile_pass(
        root,
        this_host,
        runner,
        &mut FlappingCap::default(),
        &mut debounce,
    ))
}

/// Like [`reconcile_pass`] but over IN-MEMORY specs (a single-file st2 spec's team) rather than a
/// discovered catalog — the `st2 up <spec>` path. Same reconcile/execute/flapping/liveness-debounce
/// core; `this_host` filters, `cap`/`debounce` carry state across passes.
/// One supervised pass over an in-memory spec team, carrying `cap`+`debounce` across calls (so a
/// respawn is flicker-tolerant AND flapping-capped). The `st2 up <spec>` loop uses it per interval; the
/// `supervise` eval calls it per wait-tick so a fault-injected dead seat is respawned FROM SPEC exactly
/// once (the carried cap rate-limits a same-episode second respawn; the carried debounce absorbs a
/// transient `pty list` misread of a healthy seat).
pub fn reconcile_pass_specs(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
) -> UpReport {
    let mut report = UpReport::default();
    let sessions = match runner.list_sessions() {
        Ok(s) => s,
        Err(e) => {
            report.skipped = true;
            report
                .errors
                .push(format!("list sessions (pass skipped): {e}"));
            return report;
        }
    };
    reconcile_pass_specs_with_sessions(specs, &sessions, this_host, runner, cap, debounce)
}

/// Reconcile an in-memory team against an already captured session snapshot. Eval supervision uses
/// this so crash classification and reconciliation see the same terminal state: otherwise a clean
/// process can exit between two `pty list` calls, be reaped by the second call, then look like a
/// vanished crash on the next tick.
pub(crate) fn reconcile_pass_specs_with_sessions(
    specs: &[agent_spec::spec::AgentSpec],
    sessions: &[Session],
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
) -> UpReport {
    let mut report = UpReport::default();
    let now = Instant::now();
    debounce.observe(sessions, now);
    let mut plan = crate::reconcile(specs, sessions, this_host);
    report.deferred = debounce.defer_flickers(&mut plan, now);
    execute(&plan, runner, cap, &mut report);
    report
}

/// One reconcile pass over an in-memory spec team (`st2 up <spec> --once`). Throwaway cap+debounce
/// (a single pass has no flicker history); never `Err` — failures collect in `report.errors`.
pub fn up_once_specs(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    runner: &dyn Runner,
) -> UpReport {
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    reconcile_pass_specs(
        specs,
        this_host,
        runner,
        &mut FlappingCap::default(),
        &mut debounce,
    )
}

/// One bounded task-scoped pass over already-discovered specs. Selector resolution precedes any runner call.
pub fn up_once_selected_specs(
    catalog_root: &Path,
    specs: &[crate::spec::AgentSpec],
    selector: &str,
    this_host: &str,
    runner: &dyn Runner,
) -> anyhow::Result<UpReport> {
    up_once_selected_specs_with_gates(catalog_root, specs, selector, this_host, runner, || {
        crate::hooks::verify_installed().map(|_| ())
    })
}

/// Discover a folder catalog once, resolve one task before any owner hook/render mutation, then
/// materialize only that owner and execute the selected plan.
pub fn up_once_selected(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    runner: &dyn Runner,
) -> anyhow::Result<UpReport> {
    let found = crate::discovery::discover(catalog_root);
    let (owner, _, _) = crate::reconcile::resolve_task(&found.specs, selector, this_host)?;
    let mut report = UpReport::default();
    report.warnings.extend(found.warnings);
    report.errors.extend(
        found
            .errors
            .into_iter()
            .map(|e| format!("{}: {}", e.path.display(), e.message)),
    );
    let owner = owner.clone();
    if crate::hooks::required_by_codex_agent(&owner, this_host, catalog_root)
        && let Err(error) = crate::hooks::verify_installed()
    {
        report
            .errors
            .push(format!("verify lifecycle hooks: {error}"));
        return Ok(report);
    }
    let materialized = crate::materialize::materialize_catalog(
        catalog_root,
        std::slice::from_ref(&owner),
        this_host,
    );
    report.warnings.extend(materialized.warnings);
    let owner_materialization_failed = !materialized.failed_agents.is_empty();
    report.errors.extend(materialized.errors);
    if owner_materialization_failed {
        return Ok(report);
    }
    let execution = up_once_selected_specs_with_gates(
        catalog_root,
        &found.specs,
        selector,
        this_host,
        runner,
        || Ok(()),
    )?;
    report.absorb(execution);
    Ok(report)
}

fn up_once_selected_specs_with_gates<V>(
    catalog_root: &Path,
    specs: &[crate::spec::AgentSpec],
    selector: &str,
    this_host: &str,
    runner: &dyn Runner,
    verify_hooks: V,
) -> anyhow::Result<UpReport>
where
    V: FnOnce() -> anyhow::Result<()>,
{
    crate::reconcile::resolve_task(specs, selector, this_host)?;
    let sessions = runner
        .list_sessions()
        .map_err(|e| anyhow::anyhow!("list sessions: {e}"))?;
    let mut plan = crate::reconcile::reconcile_selected(specs, &sessions, this_host, selector)?;
    let mut report = UpReport::default();
    gate_codex_launches_on_hooks(&mut plan, catalog_root, &mut report, verify_hooks);
    execute(&plan, runner, &mut FlappingCap::default(), &mut report);
    Ok(report)
}

/// Supervise an in-memory spec team: keep-alive + respawn on a timer, behaving exactly like
/// [`up_loop`] over a catalog (same
/// FlappingCap, LivenessDebounce, crash-loop surfacing, and "stop leaves sessions running"). Timer-only
/// (a spec is one static file — no folder to watch; edit + restart to change it). `root` roots
/// `$CATALOG` + crash-loop surfacing. Runs until SIGINT/SIGTERM.
pub fn up_loop_specs(
    specs: &[agent_spec::spec::AgentSpec],
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    interval: Duration,
    mut on_report: impl FnMut(&UpReport),
) -> anyhow::Result<()> {
    install_signal_handler();
    let mut cap = FlappingCap::default();
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let mut reported_flapping: HashSet<String> = HashSet::new();
    loop {
        let report = reconcile_pass_specs(specs, this_host, runner, &mut cap, &mut debounce);
        for cl in &report.crash_loops {
            if reported_flapping.insert(cl.pty_id.clone()) {
                eprintln!(
                    "st2: GAVE UP on '{}' — crash-looping past its restart{{}} policy (mode=fail); leaving it parked and its last session for inspection. Fix the cause, then restart st2.",
                    cl.pty_id
                );
                surface_crash_loop(root, this_host, cl);
            }
        }
        on_report(&report);
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        // Sleep the interval in 250ms slices so Ctrl-C is responsive (timer-only; no fs-watch).
        let slices = (interval.as_millis() / 250).max(1);
        for _ in 0..slices {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
    }
    eprintln!(
        "st2: stopping; leaving sessions running (agents are decoupled from the supervisor)."
    );
    Ok(())
}

/// Explicit teardown (`st2 down`) — kill EVERY live task of this host's catalog agents. This is the
/// one operation that ends tasks (the Nomad model: stopping the supervisor never does). Idempotent:
/// tasks already gone are simply not in the live set. Per-kill errors are collected, never fatal.
pub fn down(root: &Path, this_host: &str, runner: &dyn Runner) -> anyhow::Result<UpReport> {
    let found = crate::discover(root);
    let mut report = UpReport {
        warnings: found.warnings.clone(),
        errors: found
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.path.display(), e.message))
            .collect(),
        ..Default::default()
    };
    teardown_specs(&found.specs, this_host, runner, &mut report)?;
    Ok(report)
}

/// `st2 down` for a single-file team spec: tear down the declared team's live sessions on this host.
/// The symmetric verb to `up`/`ls` over a spec — the "stop the fleet cleanly" step of the swap runbook.
/// Sessions persist across an `st2 up` supervisor exit (nomad-decoupled), so this is how you actually
/// stop them. `specs` are the already-resolved [`AgentSpec`]s (from `spec_to_agent_specs`).
pub fn down_specs(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    runner: &dyn Runner,
) -> anyhow::Result<UpReport> {
    let mut report = UpReport::default();
    teardown_specs(specs, this_host, runner, &mut report)?;
    Ok(report)
}

/// Shared teardown core: kill every live task session declared on this host. Task session ids are
/// derived identically to how reconcile spawns them (explicit `task.id`, else `<bus_id>.<task>`), so
/// the catalog `down` and the spec `down_specs` tear down exactly what `up`/`up_*_specs` launched.
fn teardown_specs(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    runner: &dyn Runner,
    report: &mut UpReport,
) -> anyhow::Result<()> {
    let live: HashSet<String> = runner
        .list_sessions()?
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.pty_id)
        .collect();
    for spec in specs {
        if spec.resolved_host(this_host) != this_host {
            report.other_host.push(spec.identity.clone());
            continue;
        }
        let bus_id = spec.bus_id(this_host);
        for task in &spec.tasks {
            let id = task
                .id
                .clone()
                .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
            if live.contains(&id) {
                match runner.kill(&id) {
                    Ok(()) => report.torn_down.push(id),
                    Err(e) => report.errors.push(format!("kill {id}: {e}")),
                }
            }
        }
    }
    Ok(())
}

// ---- The supervisor loop (M3) ----------------------------------------------------------------

/// Set by SIGINT/SIGTERM to break the loop cleanly (agents keep running — they're decoupled).
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handler() {
    // A plain `signal()` handler is enough to flip an atomic flag; no reentrant work is done here.
    // Cast through a fn pointer (not the zero-sized fn item) to the C handler type.
    let handler = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn drain(rx: &Receiver<()>) {
    while rx.try_recv().is_ok() {}
}

/// The supervisor loop: reconcile on a timer AND on folder changes until interrupted. The fs-watch is
/// best-effort; the `interval` timer is the always-on fallback. `on_report` is called once per pass
/// (e.g. to log a summary). Returns when a stop signal arrives — agents are left running.
pub fn up_loop(
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    interval: Duration,
    on_report: impl FnMut(&UpReport),
) -> anyhow::Result<()> {
    install_signal_handler();
    up_loop_until(root, this_host, runner, interval, &STOP, on_report)
}

fn up_loop_until(
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    interval: Duration,
    stop: &AtomicBool,
    mut on_report: impl FnMut(&UpReport),
) -> anyhow::Result<()> {
    let (tx, rx) = channel::<()>();
    let _watcher = crate::watch::watch_catalog_declarations(root, tx);
    let mut cap = FlappingCap::default();
    // Carries per-id liveness across passes so a transient `pty list` flicker under load isn't
    // destructively GC'd (R21c). Fresh throwaway in `up_once` — a single pass has no flicker to absorb.
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);

    // Surface each parked crash-loop once (not every pass): an stderr line AND a message to the
    // agent's supervisor over the native bus, so a crash-loop isn't only visible to whoever is
    // watching the log.
    let mut reported_flapping: HashSet<String> = HashSet::new();

    loop {
        let report = reconcile_pass(root, this_host, runner, &mut cap, &mut debounce);
        for cl in &report.crash_loops {
            if reported_flapping.insert(cl.pty_id.clone()) {
                eprintln!(
                    "st2: GAVE UP on '{}' — crash-looping past its restart{{}} policy (mode=fail); leaving it parked and its last session for inspection. Fix the cause, then restart st2.",
                    cl.pty_id
                );
                surface_crash_loop(root, this_host, cl);
            }
        }
        on_report(&report);

        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Sleep the interval in 250ms slices, waking early on a folder change or a stop signal.
        let slices = (interval.as_millis() / 250).max(1);
        for _ in 0..slices {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(()) => {
                    drain(&rx); // coalesce a burst of events into one pass
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
    }

    eprintln!(
        "st2: stopping; leaving sessions running (agents are decoupled from the supervisor)."
    );
    Ok(())
}

/// Surface a crash-loop to the parked agent's `supervisor` over the native bus (M2.4): a one-shot
/// `crash-loop`-tagged message, so a crash-looping agent isn't only an stderr line the operator has to
/// be watching (the exact miss that let a 45-min outage run). Best-effort — a missing supervisor,
/// an unresolvable supervisor, or a send failure is logged, never fatal. Dedup (once per park) is the
/// caller's job.
pub fn surface_crash_loop(catalog_root: &Path, this_host: &str, cl: &CrashLoop) {
    let agent = cl.agent_bus_id(this_host);
    let Some(supervisor) = cl.supervisor.as_deref() else {
        eprintln!(
            "st2: crash-loop '{}' ({agent}) has no supervisor to notify.",
            cl.pty_id
        );
        return;
    };
    let Some(agent_dir) = message::resolve_agent_dir(catalog_root, supervisor, this_host) else {
        eprintln!(
            "st2: crash-loop '{}': supervisor '{supervisor}' not found in the catalog to notify.",
            cl.pty_id
        );
        return;
    };
    let subject = format!("crash-loop: {agent} parked");
    let body = format!(
        "st2 gave up restarting task '{}' (agent {agent}) — it crash-looped past its restart{{}} \
         policy (mode=fail) and is parked. Its last dead session is left as evidence. Investigate the \
         cause, then restart st2 to unpark it.",
        cl.pty_id
    );
    let from = format!("st2.{this_host}"); // the runner is the sender
    let tags = ["crash-loop".to_string()];
    if let Err(e) = message::send_to_inbox(
        &message::inbox_dir(&agent_dir),
        &from,
        Some(&subject),
        None,
        &tags,
        &body,
    ) {
        eprintln!(
            "st2: failed to notify supervisor '{supervisor}' of crash-loop '{}': {e}",
            cl.pty_id
        );
    }
}

/// Best-effort detection of this machine's short hostname (the catalog's host segment), used as the
/// default reconcile host filter. Falls back to `localhost` if it can't be determined.
pub fn detect_host() -> String {
    // `hostname` is ubiquitous; take the first dotted label (short name, e.g. `hetz`).
    if let Ok(out) = Command::new("hostname").output()
        && out.status.success()
    {
        let full = String::from_utf8_lossy(&out.stdout);
        if let Some(short) = full.trim().split('.').next()
            && !short.is_empty()
        {
            return short.to_string();
        }
    }
    "localhost".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_spec::spec::{AgentSpec, JobType, Task, TaskKind};
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    fn target(id: &str, cmd: &str) -> TaskTarget {
        TaskTarget {
            kind: TaskKind::Pty,
            pty_id: id.to_string(),
            bus_id: "hetz.demo".to_string(),
            name: "agent".to_string(),
            launch: TaskLaunch::Shell(cmd.to_string()),
            cwd: None,
            workspace: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
        }
    }

    struct GateRunner {
        list_calls: Cell<usize>,
    }

    impl Runner for GateRunner {
        fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            self.list_calls.set(self.list_calls.get() + 1);
            Ok(Vec::new())
        }

        fn spawn(&self, _target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
            panic!("gate runner must not spawn")
        }

        fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
            panic!("gate runner must not kill")
        }

        fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
            panic!("gate runner must not remove")
        }
    }

    #[test]
    fn selected_codex_gate_suppresses_launch_on_stale_hooks() {
        let spec = AgentSpec {
            identity: "codex".into(),
            host: None,
            role: None,
            job_type: JobType::Service,
            workspace: None,
            supervisor: None,
            retired: false,
            keep: false,
            restart: None,
            tasks: vec![Task {
                kind: TaskKind::Pty,
                derived: false,
                name: "agent".into(),
                id: Some("test.codex.agent".into()),
                command: None,
                argv: Some(vec!["$CATALOG/bin/codex".into(), "--version".into()]),
                cwd: None,
                tags: BTreeMap::new(),
                env: BTreeMap::new(),
                keep: false,
            }],
            path: "/tmp/spec.kdl".into(),
        };
        let runner = GateRunner {
            list_calls: Cell::new(0),
        };
        let report = up_once_selected_specs_with_gates(
            Path::new("/tmp"),
            &[spec],
            "test.codex.agent",
            "test",
            &runner,
            || anyhow::bail!("stale receipt"),
        )
        .unwrap();
        assert_eq!(runner.list_calls.get(), 1);
        assert!(report.launched.is_empty());
        assert!(report.errors.iter().any(|error| {
            error.contains("stale receipt") && error.contains("launch suppressed")
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn idle_supervisor_does_not_spin_on_its_own_catalog_reads() {
        let catalog = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let mut passes = 0usize;

        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(350));
                stop.store(true, Ordering::SeqCst);
            });
            up_loop_until(
                catalog.path(),
                "test-host",
                &GateRunner {
                    list_calls: Cell::new(0),
                },
                Duration::from_secs(60),
                &stop,
                |_| passes += 1,
            )
            .unwrap();
        });

        assert!(
            passes <= 2,
            "idle supervisor must wait instead of reconciling its own read events: {passes} passes"
        );
    }

    // ── liveness debounce (R21c): a transient `pty list` not-alive flicker under load must not
    //    destructively GC/relaunch a HEALTHY agent; a stable death must still be reaped ──────────────

    use crate::reconcile::Launch;
    fn sess(id: &str, alive: bool) -> Session {
        Session {
            pty_id: id.to_string(),
            alive,
            exit_code: None,
        }
    }

    fn spec_fixture() -> AgentSpec {
        AgentSpec {
            identity: "demo".into(),
            host: Some("hetz".into()),
            role: None,
            job_type: JobType::Service,
            workspace: None,
            supervisor: None,
            retired: false,
            keep: false,
            restart: None,
            tasks: vec![],
            path: std::path::PathBuf::from("/x"),
        }
    }

    #[test]
    fn debounce_absorbs_a_gc_flicker_but_reaps_a_stable_death() {
        let t0 = Instant::now();
        let mut db = LivenessDebounce::new(Duration::from_secs(10));
        db.observe(&[sess("hetz.demo.agent", true)], t0);

        // Flicker: reads not-alive 1s later but was alive within the grace → deferred (left running).
        let mut plan = ReconcilePlan::default();
        plan.gc.push("hetz.demo.agent".into());
        let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(1));
        assert!(
            plan.gc.is_empty(),
            "a recently-alive flicker must NOT be GC'd"
        );
        assert_eq!(deferred, vec!["hetz.demo.agent".to_string()]);

        // CENTRAL anti-over-correction check: a STABLE death past the grace IS still reaped — the
        // debounce must never MASK a real death.
        let mut plan = ReconcilePlan::default();
        plan.gc.push("hetz.demo.agent".into());
        let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(11));
        assert_eq!(
            plan.gc,
            vec!["hetz.demo.agent".to_string()],
            "a stable death must still be reaped"
        );
        assert!(deferred.is_empty());
    }

    #[test]
    fn effective_pty_root_prefers_an_exported_ambient_root_else_catalog_pty() {
        let cat = std::path::Path::new("/deep/sandbox/st-root");
        // No ambient PTY_ROOT → the rendered default `<catalog>/pty`.
        assert_eq!(effective_pty_root_from(cat, None), cat.join("pty"));
        assert_eq!(
            effective_pty_root_from(cat, Some("".into())),
            cat.join("pty"),
            "empty is treated as unset"
        );
        // An exported ambient PTY_ROOT (e.g. an eval's short decoupled root) WINS — so a deep catalog
        // path can't blow the unix-socket limit, and spawn agrees with list/kill.
        let short = std::ffi::OsString::from("/tmp/stev-abc123");
        assert_eq!(
            effective_pty_root_from(cat, Some(short)),
            std::path::PathBuf::from("/tmp/stev-abc123")
        );
    }

    #[test]
    fn a_catalog_declared_root_outranks_the_default_but_never_an_ambient_one() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = tmp.path();
        std::fs::write(
            cat.join(crate::catalog::CONFIG_FILE),
            "catalog { pty-root \"/run/agents/pty\" }\n",
        )
        .unwrap();

        // The declaration replaces the `<catalog>/pty` default for every st2 pty op — so a reader
        // that resolves the catalog finds the sessions without being handed an env var.
        assert_eq!(
            effective_pty_root_from(cat, None),
            std::path::PathBuf::from("/run/agents/pty")
        );
        // An explicit ambient root still wins: an eval run's short decoupled partition must be able
        // to override a catalog it copied from.
        assert_eq!(
            effective_pty_root_from(cat, Some("/tmp/stev-abc123".into())),
            std::path::PathBuf::from("/tmp/stev-abc123")
        );
    }

    #[test]
    fn debounce_never_defers_a_never_seen_task() {
        let t0 = Instant::now();
        let db = LivenessDebounce::new(Duration::from_secs(10));
        // A genuinely-new target (never observed alive) is handled immediately, not deferred.
        let mut plan = ReconcilePlan::default();
        plan.gc.push("hetz.brandnew.agent".into());
        let deferred = db.defer_flickers(&mut plan, t0);
        assert_eq!(plan.gc, vec!["hetz.brandnew.agent".to_string()]);
        assert!(deferred.is_empty());
    }

    #[test]
    fn debounce_defers_a_flickering_launch_target_too() {
        let t0 = Instant::now();
        let mut db = LivenessDebounce::new(Duration::from_secs(10));
        db.observe(&[sess("hetz.demo.agent", true)], t0);

        // The same recently-alive id showing up as a launch target (Absent/Dead) is also deferred —
        // no noisy "already in use" re-launch of a live session.
        let spec = spec_fixture();
        let mut plan = ReconcilePlan::default();
        plan.launch.push(Launch {
            spec: &spec,
            tasks: vec![target("hetz.demo.agent", "x")],
        });
        let deferred = db.defer_flickers(&mut plan, t0 + Duration::from_secs(2));
        assert!(
            plan.launch.is_empty(),
            "a recently-alive flicker must NOT be re-launched"
        );
        assert_eq!(deferred, vec!["hetz.demo.agent".to_string()]);
    }

    #[test]
    fn codex_hook_gate_accepts_new_agents_without_mutating_the_launch_plan() {
        let mut left = spec_fixture();
        left.identity = "left".into();
        left.path = PathBuf::from("/catalog/node/left/agent.kdl");
        let mut right = spec_fixture();
        right.identity = "right".into();
        right.path = PathBuf::from("/catalog/node/right/agent.kdl");
        let mut left_agent = target("node.left.agent", "exec codex --model gpt-5");
        left_agent.workspace = Some("/workspaces/shared".into());
        let mut right_agent = target("node.right.agent", "/opt/bin/codex --model gpt-5");
        right_agent.workspace = Some("/workspaces/shared".into());
        let mut plan = ReconcilePlan::default();
        plan.launch.push(Launch {
            spec: &left,
            tasks: vec![left_agent],
        });
        plan.launch.push(Launch {
            spec: &right,
            tasks: vec![right_agent],
        });
        let expected = plan
            .launch
            .iter()
            .map(|launch| launch.spec.identity.clone())
            .collect::<Vec<_>>();
        let mut report = UpReport::default();

        gate_codex_launches_on_hooks(
            &mut plan,
            Path::new("/catalog"),
            &mut report,
            || Ok(()),
        );

        assert_eq!(
            plan.launch
                .iter()
                .map(|launch| launch.spec.identity.clone())
                .collect::<Vec<_>>(),
            expected,
            "successful hook verification must leave the launch plan unchanged"
        );
        assert_eq!(plan.launch.len(), 2);
        assert!(report.errors.is_empty());
    }

    #[test]
    fn codex_hook_gate_does_not_touch_adopted_agents_or_sidecar_only_repairs() {
        let mut spec = spec_fixture();
        spec.identity = "root".into();
        let mut ding = target("node.root.ding", "st2 ding");
        ding.name = "ding".into();
        let mut plan = ReconcilePlan::default();
        plan.adopt.push(&spec);
        plan.launch.push(Launch {
            spec: &spec,
            tasks: vec![ding],
        });
        let mut report = UpReport::default();

        gate_codex_launches_on_hooks(
            &mut plan,
            Path::new("/catalog"),
            &mut report,
            || panic!("an already-live Codex agent must not enter the hook gate"),
        );

        assert_eq!(plan.adopt, [&spec]);
        assert_eq!(plan.launch.len(), 1);
        assert_eq!(plan.launch[0].tasks[0].name, "ding");
        assert!(report.errors.is_empty());
    }

    #[test]
    fn hook_verification_failure_suppresses_only_new_codex_agents() {
        let mut codex = spec_fixture();
        codex.identity = "codex".into();
        codex.path = PathBuf::from("/catalog/node/codex/agent.kdl");
        let mut claude = spec_fixture();
        claude.identity = "claude".into();
        claude.path = PathBuf::from("/catalog/node/claude/agent.kdl");
        let mut codex_agent = target("node.codex.agent", "exec codex");
        codex_agent.workspace = Some("/workspaces/codex".into());
        let claude_agent = target("node.claude.agent", "exec claude");
        let mut plan = ReconcilePlan::default();
        plan.launch.push(Launch {
            spec: &codex,
            tasks: vec![codex_agent],
        });
        plan.launch.push(Launch {
            spec: &claude,
            tasks: vec![claude_agent],
        });
        let mut report = UpReport::default();

        gate_codex_launches_on_hooks(
            &mut plan,
            Path::new("/catalog"),
            &mut report,
            || anyhow::bail!("stale receipt"),
        );

        assert_eq!(
            plan.launch
                .iter()
                .map(|launch| launch.spec.identity.as_str())
                .collect::<Vec<_>>(),
            ["claude"]
        );
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("stale receipt"));
        assert!(report.errors[0].contains("launch suppressed"));
    }

    /// The built `pty run` argv runs the command verbatim under `sh -c`, detached, with the pinned id
    /// and the owning agent's bus identity as its human-facing name.
    #[test]
    fn build_run_command_wraps_command_in_sh_c() {
        let cli = PtyCli::default();
        let t = target(
            "hetz.demo.agent",
            "exec claude --permission-mode bypassPermissions 'boot'",
        );
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

        assert_eq!(cmd.get_program(), OsStr::new("pty"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // Stable launch/config arguments precede the persisted environment and command separator.
        assert_eq!(&args[0..2], &["run", "-d"]);
        assert!(args.contains(&"--force".to_string()));
        let id_pos = args.iter().position(|a| a == "--id").unwrap();
        assert_eq!(args[id_pos + 1], "hetz.demo.agent");
        let name_pos = args.iter().position(|a| a == "--name").unwrap();
        assert_eq!(args[name_pos + 1], "hetz.demo");
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &args[sep + 1..],
            &[
                "sh",
                "-c",
                "exec claude --permission-mode bypassPermissions 'boot'"
            ]
        );
    }

    #[test]
    fn build_run_command_passes_direct_argv_without_a_shell() {
        let cli = PtyCli::new(PathBuf::from("/my/catalog"));
        let mut t = target("hetz.demo.agent", "unused");
        t.launch = TaskLaunch::Argv(vec![
            "axe".into(),
            "agent".into(),
            "exec".into(),
            "--".into(),
            "claude".into(),
            "--resume".into(),
            "$CATALOG/session id".into(),
        ]);
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let sep = args.iter().position(|arg| arg == "--").unwrap();

        assert_eq!(
            &args[sep + 1..],
            [
                "axe",
                "agent",
                "exec",
                "--",
                "claude",
                "--resume",
                "/my/catalog/session id"
            ]
        );
        assert!(!args[sep + 1..].iter().any(|arg| arg == "sh"));
    }

    #[test]
    fn build_run_command_persists_the_complete_managed_environment_before_the_command() {
        let cli = PtyCli::new(PathBuf::from("/my/catalog"));
        let mut t = target("hetz.demo.agent", "exec codex 'boot'");
        t.env.insert("CUSTOM".into(), "task-value".into());
        t.env.insert("ST_AGENT".into(), "hetz.demo".into());
        t.env.insert("ST_ROOT".into(), "$CATALOG/custom-bus".into());
        t.env.insert("TERM".into(), "screen-256color".into());
        t.env
            .insert("PTY_ROOT".into(), "/declared/root/must-not-win".into());
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

        let args: Vec<String> = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        let mut persisted = BTreeMap::new();
        let mut index = 0;
        while index < separator {
            if args[index] == "--env" {
                let (key, value) = args[index + 1].split_once('=').unwrap();
                assert!(
                    persisted
                        .insert(key.to_string(), value.to_string())
                        .is_none(),
                    "the final managed overlay needs only one persisted value per key"
                );
                index += 2;
            } else {
                index += 1;
            }
        }
        let inherited = cmd
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.unwrap().to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            persisted, inherited,
            "initial process env and restart-persisted env must be the same resolved overlay"
        );
        assert_eq!(
            persisted.get("CATALOG").map(String::as_str),
            Some("/my/catalog")
        );
        assert_eq!(
            persisted.get("ST_ROOT").map(String::as_str),
            Some("/my/catalog/custom-bus")
        );
        assert_eq!(
            persisted.get("PTY_ROOT").map(String::as_str),
            Some(
                effective_pty_root(&cli.catalog_root)
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert_eq!(
            persisted.get("TERM").map(String::as_str),
            Some("screen-256color")
        );
        assert_eq!(
            persisted.get("ST_AGENT").map(String::as_str),
            Some("hetz.demo")
        );
        assert_eq!(
            persisted.get("CUSTOM").map(String::as_str),
            Some("task-value")
        );
        assert!(persisted.contains_key("ST_HOOKS"));
    }

    #[test]
    fn build_run_command_omits_an_alias_equal_to_the_lifecycle_id() {
        let cli = PtyCli::default();
        let mut t = target("hetz.demo", "exec codex 'boot'");
        t.bus_id = t.pty_id.clone();
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args.iter()
                .position(|arg| arg == "--id")
                .map(|position| args[position + 1].as_str()),
            Some("hetz.demo")
        );
        assert!(
            !args.iter().any(|arg| arg == "--name"),
            "pty rejects a display name equal to the stable session id"
        );
        assert!(
            args.iter().any(|arg| arg == "--no-display-name"),
            "without this flag pty would create an unrelated automatic alias"
        );
    }

    #[test]
    fn build_run_command_defaults_cwd_to_spec_dir_and_passes_tags_and_env() {
        let cli = PtyCli::default();
        let mut t = target("hetz.demo.agent", "exec claude 'boot'");
        t.tags.insert("role".into(), "agent".into());
        t.env.insert("ST_AGENT".into(), "hetz.demo-claude".into());
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
        assert_eq!(args[cwd_pos + 1], "/cat/hetz/demo"); // no cwd, no workspace → spec dir
        let tag_pos = args.iter().position(|a| a == "--tag").unwrap();
        assert_eq!(args[tag_pos + 1], "role=agent");

        // env injected onto the child process
        let envs: BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("ST_AGENT"),
            Some(&Some("hetz.demo-claude".to_string()))
        );
        assert_eq!(
            envs.get("TERM"),
            Some(&Some("xterm-256color".to_string())),
            "headless st2 launches must not pass TERM=dumb into an interactive harness"
        );
        assert!(
            envs.get("ST_HOOKS")
                .and_then(Option::as_deref)
                .is_some_and(|path| !path.contains("/sets/sha256-")),
            "managed tasks keep ST_HOOKS at the receipt-bearing root; only rendered hook commands use a versioned set"
        );
    }

    #[test]
    fn build_run_command_allows_a_task_to_override_the_default_term() {
        let cli = PtyCli::default();
        let mut t = target("hetz.demo.agent", "exec codex 'boot'");
        t.env.insert("TERM".into(), "screen-256color".into());
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let term = cmd
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("TERM"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(term.as_deref(), Some("screen-256color"));
    }

    #[test]
    fn build_run_command_defaults_cwd_to_workspace_when_task_cwd_absent() {
        let cli = PtyCli::default();
        let mut t = target("hetz.demo.agent", "exec claude 'boot'");
        t.workspace = Some("/repos/demo".into()); // no task cwd → workspace (spec.md §2)
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
        assert_eq!(args[cwd_pos + 1], "/repos/demo");
    }

    #[test]
    fn build_run_command_expands_catalog_var_and_sets_it_in_env() {
        let cli = PtyCli::new(PathBuf::from("/my/catalog"));
        let mut t = target("hetz.demo.agent", "run");
        t.env.insert("DATA".into(), "$CATALOG/evals/x".into());
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let envs: BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("DATA"),
            Some(&Some("/my/catalog/evals/x".to_string()))
        );
        assert_eq!(envs.get("CATALOG"), Some(&Some("/my/catalog".to_string())));
    }

    #[test]
    fn build_run_command_expands_vars_in_env_cwd_and_tags_but_not_command() {
        // Unique var name so the process-global set_var can't collide with a parallel test.
        let key = "ST2_TEST_EXPAND_NET_9f3";
        unsafe { std::env::set_var(key, "/net/xyz") }

        let cli = PtyCli::default();
        let mut t = target("hetz.demo.agent", "exec claude $ST2_TEST_EXPAND_NET_9f3/go");
        t.cwd = Some(format!("${key}/work"));
        t.tags.insert("net".into(), format!("${key}"));
        t.env.insert("ST_ROOT".into(), format!("${key}/custom-bus"));
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // cwd expanded (absolute → replaces the spec dir)
        let cwd_pos = args.iter().position(|a| a == "--cwd").unwrap();
        assert_eq!(args[cwd_pos + 1], "/net/xyz/work");
        // tag value expanded
        let tag_pos = args.iter().position(|a| a == "--tag").unwrap();
        assert_eq!(args[tag_pos + 1], "net=/net/xyz");
        // command left verbatim for sh -c to expand at spawn
        assert_eq!(
            args.last().unwrap(),
            "exec claude $ST2_TEST_EXPAND_NET_9f3/go"
        );

        // env value expanded
        let envs: std::collections::BTreeMap<String, Option<String>> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs.get("ST_ROOT"),
            Some(&Some("/net/xyz/custom-bus".to_string()))
        );

        unsafe { std::env::remove_var(key) }
    }

    #[test]
    fn resolve_cwd_honors_relative_absolute_workspace_and_default() {
        let cli = PtyCli::default();
        let mut t = target("x", "y");
        // relative cwd → joined onto the spec dir
        t.cwd = Some("sub".into());
        assert_eq!(
            cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
            Path::new("/cat/hetz/demo/sub")
        );
        // absolute cwd → replaces
        t.cwd = Some("/repos/fabric".into());
        assert_eq!(
            cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
            Path::new("/repos/fabric")
        );
        // no cwd but a workspace → workspace
        t.cwd = None;
        t.workspace = Some("/repos/ws".into());
        assert_eq!(
            cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
            Path::new("/repos/ws")
        );
        // neither → spec dir
        t.workspace = None;
        assert_eq!(
            cli.resolve_cwd(&t, Path::new("/cat/hetz/demo")),
            Path::new("/cat/hetz/demo")
        );
    }

    #[test]
    fn detect_host_returns_a_nonempty_short_name() {
        let h = detect_host();
        assert!(!h.is_empty());
        assert!(!h.contains('.'), "short name only, got {h}");
    }
}
