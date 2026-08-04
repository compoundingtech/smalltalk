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
use std::fs::File;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::exec_backend::ExecBackend;
use crate::flapping::FlappingCap;
use crate::message;
use crate::reconcile::{PtyPresentation, ReconcilePlan, Session, TaskLaunch, TaskTarget};
use crate::task_inventory::{
    DesiredRuntime, ObservationBatch, ObservedState, RuntimeGeneration, RuntimeObservation,
    RuntimeObserver, generation_id,
};
use agent_spec::spec::TaskKind;

// This is an outer containment bound for a wedged runtime, not a fleet-scalability mechanism.
const PTY_LIST_TIMEOUT: Duration = Duration::from_secs(2);
const PTY_DAEMON_SHUTDOWN_WAIT: Duration = Duration::from_secs(6);
const MAX_PRESENTATION_PATCHES_PER_PASS: usize = 8;

#[derive(Debug, Default)]
pub(crate) struct PresentationPatchCursor {
    after_id: Option<String>,
}

impl PresentationPatchCursor {
    fn batch<'a>(&mut self, presentation: &'a [PtyPresentation]) -> Vec<&'a PtyPresentation> {
        let mut ordered = presentation.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.pty_id.cmp(&right.pty_id));
        if ordered.is_empty() {
            return Vec::new();
        }
        let start = self.after_id.as_ref().map_or(0, |after_id| {
            let next = ordered.partition_point(|item| item.pty_id <= *after_id);
            if next == ordered.len() { 0 } else { next }
        });
        let batch = (0..ordered.len().min(MAX_PRESENTATION_PATCHES_PER_PASS))
            .map(|offset| ordered[(start + offset) % ordered.len()])
            .collect::<Vec<_>>();
        self.after_id = batch.last().map(|item| item.pty_id.clone());
        batch
    }
}

/// Run a non-interactive child with bounded output capture. Regular temporary files keep an escaped
/// descendant that inherited stdout/stderr from blocking cleanup after the direct child times out.
/// The child still gets a fresh process group so the common wrapper-and-descendants case is reaped.
fn output_with_timeout(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    output_with_input_timeout(command, timeout, None)
}

fn terminate_and_reap_before(mut child: Child, pid: i32, deadline: Instant) {
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            Ok(None) | Err(_) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return;
            }
        }
    }
}

fn write_all_before(
    mut stdin: ChildStdin,
    mut input: &[u8],
    deadline: Instant,
) -> anyhow::Result<bool> {
    let fd = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error()).context("read metadata stdin flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error()).context("make metadata stdin nonblocking");
    }
    while !input.is_empty() {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match stdin.write(input) {
            Ok(0) => {
                return Err(std::io::Error::from(std::io::ErrorKind::WriteZero))
                    .context("write metadata patch payload");
            }
            Ok(written) => input = &input[written..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(20)),
                );
            }
            Err(error) => return Err(error).context("write metadata patch payload"),
        }
    }
    Ok(true)
}

fn output_with_input_timeout(
    command: &mut Command,
    timeout: Duration,
    input: Option<Vec<u8>>,
) -> anyhow::Result<Output> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    if let Some(input) = input {
        let Some(stdin) = child.stdin.take() else {
            terminate_and_reap_before(child, pid, deadline);
            anyhow::bail!("metadata patch child has no piped stdin");
        };
        match write_all_before(stdin, &input, deadline) {
            Ok(true) => {}
            Ok(false) => {
                terminate_and_reap_before(child, pid, deadline);
                anyhow::bail!("timed out after {:.1}s", timeout.as_secs_f64());
            }
            Err(error) => {
                terminate_and_reap_before(child, pid, deadline);
                return Err(error);
            }
        }
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate_and_reap_before(child, pid, deadline);
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
    /// Atomically reconcile display metadata and the complete st2-owned tag snapshot for one exact
    /// existing PTY ID. The default is a no-op for non-PTY test/backends.
    fn patch_presentation(&self, _presentation: &PtyPresentation) -> anyhow::Result<()> {
        Ok(())
    }
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
    /// PTY daemon PID. Together with `createdAt`, this identifies one generation.
    #[serde(default)]
    pid: Option<u32>,
    /// PTY-owned generation creation time.
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct PtyMetadataPatch<'a> {
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    display_name: Option<&'a Option<String>>,
    tags: &'a BTreeMap<String, Option<String>>,
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
    /// `$VAR`s are expanded here for task-authored values that do NOT pass through a shell — env,
    /// tags, `cwd`, and direct argv — because `pty` passes them through verbatim. The st2-owned
    /// presentation snapshot remains literal so initial spawn and later metadata patches agree.
    /// Shell source is left unexpanded: `sh -c` expands it at spawn from the same env.
    fn build_run_command(&self, target: &TaskTarget, spec_dir: &Path) -> Command {
        let cwd = self.resolve_cwd(target, spec_dir);
        let mut cmd = Command::new(&self.bin);
        cmd.arg("run")
            .arg("-d") // detached: leave it running in the background
            .arg("--force") // st2 itself may run inside a pty session; allow nesting
            .args(["--id", &target.pty_id]);
        match target
            .presentation
            .as_ref()
            .map(|presentation| &presentation.display_name)
        {
            Some(Some(Some(name))) if name == &target.pty_id => {
                cmd.arg("--no-display-name");
            }
            Some(Some(Some(name))) => {
                cmd.args(["--name", name]);
            }
            Some(Some(None)) => {
                cmd.arg("--no-display-name");
            }
            // Secondary tasks retain the established task-specific presentation convention.
            _ if target.pty_id == target.bus_id => {
                cmd.arg("--no-display-name");
            }
            _ => {
                cmd.args(["--name", &target.bus_id]);
            }
        }
        cmd.arg("--cwd").arg(&cwd);
        let mut tags = target
            .tags
            .iter()
            .map(|(key, value)| (key.clone(), self.expand(value)))
            .collect::<BTreeMap<_, _>>();
        if let Some(presentation) = &target.presentation {
            for (key, value) in &presentation.tags {
                match value {
                    Some(value) => {
                        tags.insert(key.clone(), value.clone());
                    }
                    None => {
                        tags.remove(key);
                    }
                }
            }
        }
        for (k, v) in &tags {
            cmd.arg("--tag").arg(format!("{k}={v}"));
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
        // Coding-agent command runners commonly set NO_COLOR for their own captured output. That
        // ambient preference belongs to the launcher, not to the interactive agent it happens to
        // reconcile. Agent Spec env remains authoritative when an agent deliberately opts out.
        if target.name == "agent" && !target.env.contains_key("NO_COLOR") {
            cmd.env_remove("NO_COLOR");
            cmd.arg("--unset-env").arg("NO_COLOR");
        }
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

    /// Pure, typed PTY observation. A missing root is known empty and is not
    /// passed to `pty`, because observation must not create it.
    fn task_observations(&self, desired_ids: &HashSet<&str>) -> ObservationBatch {
        let root = effective_pty_root(&self.catalog_root);
        self.task_observations_at_root(desired_ids, &root)
    }

    fn task_observations_at_root(
        &self,
        desired_ids: &HashSet<&str>,
        root: &Path,
    ) -> ObservationBatch {
        if desired_ids.is_empty() {
            return ObservationBatch {
                complete: true,
                ..ObservationBatch::default()
            };
        }
        // Retain the admitted directory inode across the external probe. `pty
        // list` creates PTY_ROOT when absent, so a path removed and recreated
        // during the call must never be confused with the admitted registry.
        let root_handle = match File::open(root) {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ObservationBatch {
                    complete: true,
                    ..ObservationBatch::default()
                };
            }
            Err(error) => {
                return ObservationBatch {
                    complete: false,
                    observations: Vec::new(),
                    errors: vec![format!(
                        "cannot inspect PTY root {}: {error}",
                        root.display()
                    )],
                };
            }
        };
        let metadata = match root_handle.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return ObservationBatch {
                    complete: false,
                    observations: Vec::new(),
                    errors: vec![format!(
                        "cannot inspect admitted PTY root {}: {error}",
                        root.display()
                    )],
                };
            }
        };
        if !metadata.is_dir() {
            return ObservationBatch {
                complete: false,
                observations: Vec::new(),
                errors: vec![format!("PTY root {} is not a directory", root.display())],
            };
        }
        let entries = match self.list_entries_at(root) {
            Ok(entries) => entries,
            Err(error) => {
                return ObservationBatch {
                    complete: false,
                    observations: Vec::new(),
                    errors: vec![error.to_string()],
                };
            }
        };
        let final_metadata = match std::fs::metadata(root) {
            Ok(final_metadata) => final_metadata,
            Err(error) => {
                return ObservationBatch {
                    complete: false,
                    observations: Vec::new(),
                    errors: vec![format!(
                        "PTY root {} disappeared during observation: {error}",
                        root.display()
                    )],
                };
            }
        };
        if !final_metadata.is_dir()
            || final_metadata.dev() != metadata.dev()
            || final_metadata.ino() != metadata.ino()
        {
            return ObservationBatch {
                complete: false,
                observations: Vec::new(),
                errors: vec![format!(
                    "PTY root {} changed identity during observation",
                    root.display()
                )],
            };
        }
        let mut observations = Vec::with_capacity(entries.len());
        let mut errors = Vec::new();
        for entry in entries {
            if !desired_ids.contains(entry.name.as_str()) {
                continue;
            }
            let state = match entry.status.as_str() {
                "running" => match (entry.pid, entry.created_at.as_deref()) {
                    (Some(pid), Some(created_at)) => {
                        let generation_id =
                            generation_id("pty", &entry.name, pid, created_at, None);
                        match RuntimeGeneration::new(pid, created_at.to_owned(), generation_id) {
                            Ok(generation) => ObservedState::Running(generation),
                            Err(error) => {
                                let message = format!(
                                    "invalid PTY task {:?} generation: {error}",
                                    entry.name
                                );
                                errors.push(message.clone());
                                ObservedState::Indeterminate(message)
                            }
                        }
                    }
                    _ => {
                        let message = format!(
                            "running PTY task {:?} lacks pid or createdAt generation evidence",
                            entry.name
                        );
                        errors.push(message.clone());
                        ObservedState::Indeterminate(message)
                    }
                },
                "exited" => ObservedState::Exited,
                "vanished" => ObservedState::Vanished,
                other => {
                    let message = format!("PTY task {:?} has unknown status {other:?}", entry.name);
                    errors.push(message.clone());
                    ObservedState::Indeterminate(message)
                }
            };
            observations.push(RuntimeObservation {
                runtime_id: entry.name,
                state,
            });
        }
        ObservationBatch {
            complete: errors.is_empty(),
            observations,
            errors,
        }
    }

    fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(&PtyMetadataPatch {
            display_name: presentation.display_name.as_ref(),
            tags: &presentation.tags,
        })?;
        let out = output_with_input_timeout(
            Command::new(&self.bin)
                .args(["metadata", "patch", "--id", &presentation.pty_id])
                .env("PTY_ROOT", effective_pty_root(&self.catalog_root)),
            PTY_LIST_TIMEOUT,
            Some(payload),
        )
        .map_err(|error| anyhow::anyhow!("`pty metadata patch --id` failed: {error}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty metadata patch --id {}` failed: {}",
                presentation.pty_id,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn list_entries(&self) -> anyhow::Result<Vec<PtyListEntry>> {
        self.list_entries_at(&effective_pty_root(&self.catalog_root))
    }

    fn list_entries_at(&self, root: &Path) -> anyhow::Result<Vec<PtyListEntry>> {
        let out = output_with_timeout(
            Command::new(&self.bin)
                .args(["list", "--json"])
                .env("PTY_ROOT", root),
            PTY_LIST_TIMEOUT,
        )
        .map_err(|error| anyhow::anyhow!("`pty list --json` failed: {error}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty list --json` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|error| anyhow::anyhow!("parsing `pty list --json`: {error}"))
    }
}

/// Apply both assignments and removals from an inner command to its isolation wrapper.
fn apply_command_env(source: &Command, target: &mut Command) {
    for (key, value) in source.get_envs() {
        match value {
            Some(value) => {
                target.env(key, value);
            }
            None => {
                target.env_remove(key);
            }
        }
    }
}

impl Runner for PtyCli {
    fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
        Ok(self
            .list_entries()?
            .into_iter()
            .map(|e| Session {
                pty_id: e.name,
                alive: e.status == "running",
                exit_code: e.exit_code,
                presentation: Some(crate::reconcile::ObservedPtyPresentation {
                    display_name: e.display_name,
                    tags: e.tags,
                }),
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
            apply_command_env(&inner, &mut cmd);
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

    fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
        PtyCli::patch_presentation(self, presentation)
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

impl RuntimeObserver for SystemRunner {
    fn observe(&self, desired: &[DesiredRuntime]) -> ObservationBatch {
        let pty_ids = desired
            .iter()
            .filter(|runtime| runtime.kind == TaskKind::Pty)
            .map(|runtime| runtime.runtime_id.as_str())
            .collect::<HashSet<_>>();
        let mut batch = self.pty.task_observations(&pty_ids);

        for runtime in desired
            .iter()
            .filter(|runtime| runtime.kind == TaskKind::Exec)
        {
            match self.exec.observe_generation_optional(&runtime.runtime_id) {
                Ok(None) => {
                    // Positive absence is filled by the declaration/runtime join.
                }
                Ok(Some(crate::exec_backend::ExecGenerationObservation::Running {
                    pid,
                    created_at,
                    generation_id,
                })) => {
                    let state = match RuntimeGeneration::new(pid, created_at, generation_id) {
                        Ok(generation) => ObservedState::Running(generation),
                        Err(error) => {
                            let message = format!(
                                "invalid exec task {:?} generation: {error}",
                                runtime.runtime_id
                            );
                            batch.errors.push(message.clone());
                            ObservedState::Indeterminate(message)
                        }
                    };
                    batch.observations.push(RuntimeObservation {
                        runtime_id: runtime.runtime_id.clone(),
                        state,
                    });
                }
                Ok(Some(crate::exec_backend::ExecGenerationObservation::Exited { .. })) => {
                    batch.observations.push(RuntimeObservation {
                        runtime_id: runtime.runtime_id.clone(),
                        state: ObservedState::Exited,
                    });
                }
                Ok(Some(crate::exec_backend::ExecGenerationObservation::Indeterminate {
                    reason,
                    ..
                })) => {
                    let message = format!(
                        "exec task {:?} is indeterminate: {reason}",
                        runtime.runtime_id
                    );
                    batch.errors.push(message.clone());
                    batch.observations.push(RuntimeObservation {
                        runtime_id: runtime.runtime_id.clone(),
                        state: ObservedState::Indeterminate(message),
                    });
                }
                Err(error) => {
                    let message = format!("observe exec task {:?}: {error:#}", runtime.runtime_id);
                    batch.errors.push(message.clone());
                    batch.observations.push(RuntimeObservation {
                        runtime_id: runtime.runtime_id.clone(),
                        state: ObservedState::Indeterminate(message),
                    });
                }
            }
        }
        batch.complete &= batch.errors.is_empty();
        batch
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

    fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
        self.pty.patch_presentation(presentation)
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
    /// Task IDs that st2 started without a restart reap in this pass.
    pub launched: Vec<String>,
    /// Task IDs that st2 restarted successfully in this pass. st2 reaped a dead active record
    /// before it spawned the replacement. These IDs are not first launches or final garbage
    /// collection.
    pub restarted: Vec<String>,
    /// pty ids torn down (retired agents) this pass.
    pub torn_down: Vec<String>,
    /// Task IDs in final garbage collection. st2 did not spawn replacements.
    pub gc: Vec<String>,
    /// pty ids whose GC/relaunch was DEFERRED this pass by the liveness debounce — a task that read
    /// not-alive but was alive within the grace window, i.e. a transient `pty list` flicker under load,
    /// left alone rather than destructively reaped (R21c). Not "noteworthy" (it's a no-op by design).
    pub deferred: Vec<String>,
    /// dead or absent adopt-only task ids held without reap or launch.
    pub held: Vec<String>,
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
        self.restarted.append(&mut other.restarted);
        self.torn_down.append(&mut other.torn_down);
        self.gc.append(&mut other.gc);
        self.deferred.append(&mut other.deferred);
        self.held.append(&mut other.held);
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
            || !self.restarted.is_empty()
            || !self.torn_down.is_empty()
            || !self.gc.is_empty()
            || !self.flapping.is_empty()
            || !self.warnings.is_empty()
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
    execute_with_presentation_cursor(
        plan,
        runner,
        cap,
        &mut PresentationPatchCursor::default(),
        report,
    );
}

fn stop_live_derived_companions(
    launch: &crate::reconcile::Launch<'_>,
    runner: &dyn Runner,
    report: &mut UpReport,
) {
    for companion_id in &launch.live_derived {
        match runner.kill(companion_id) {
            Ok(()) => report.torn_down.push(companion_id.clone()),
            Err(error) => report
                .errors
                .push(format!("kill unavailable derived companion {companion_id}: {error}")),
        }
    }
}

fn execute_with_presentation_cursor(
    plan: &ReconcilePlan,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    presentation_cursor: &mut PresentationPatchCursor,
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
        let launched_agent = launch
            .tasks
            .iter()
            .find(|target| target.name == "agent" && !target.derived);
        let mut agent_available = launched_agent.is_none();
        let ordered_targets = launched_agent.into_iter().chain(
            launch
                .tasks
                .iter()
                .filter(|target| launched_agent.is_none_or(|agent| agent.pty_id != target.pty_id)),
        );
        for target in ordered_targets {
            if target.derived && !agent_available {
                continue;
            }
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
                    if target.name == "agent" && !target.derived {
                        agent_available = false;
                        stop_live_derived_companions(launch, runner, report);
                    }
                    continue;
                }
                // Delaying / RateLimited: transient — skip quietly, retry a later pass, keep the corpse.
                crate::flapping::RestartDecision::Delaying
                | crate::flapping::RestartDecision::RateLimited => {
                    if target.name == "agent" && !target.derived {
                        agent_available = false;
                    }
                    continue;
                }
            }
            // Reap the dead record before st2 starts a replacement. A dead record blocks the
            // replacement. The backend preserves its bounded diagnostics.
            let restarting = gc_set.contains(target.pty_id.as_str());
            if restarting {
                match runner.reap_for_restart(&target.pty_id) {
                    Ok(()) => {}
                    Err(e) => {
                        report
                            .errors
                            .push(format!("reap {} for restart: {e}", target.pty_id));
                        if target.name == "agent" && !target.derived {
                            agent_available = false;
                            stop_live_derived_companions(launch, runner, report);
                        }
                        continue;
                    }
                }
            }
            match runner.spawn(target, spec_dir) {
                Ok(()) => {
                    cap.record(&target.pty_id, now);
                    if restarting {
                        report.restarted.push(target.pty_id.clone());
                    } else {
                        report.launched.push(target.pty_id.clone());
                    }
                    if target.name == "agent" && !target.derived {
                        agent_available = true;
                    }
                }
                Err(e) => {
                    report.errors.push(format!("spawn {}: {e}", target.pty_id));
                    if target.name == "agent" && !target.derived {
                        agent_available = false;
                        stop_live_derived_companions(launch, runner, report);
                    }
                }
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

    // Presentation never delays lifecycle convergence. Drift repair is bounded to eight sequential
    // children, keeping its worst-case 2s-per-child containment below the 30s supervisor cadence;
    // remaining drift is observed and retried on later passes.
    for presentation in presentation_cursor.batch(&plan.presentation) {
        if let Err(error) = runner.patch_presentation(presentation) {
            report
                .errors
                .push(format!("metadata patch {}: {error}", presentation.pty_id));
        }
    }
    let deferred_presentation = plan
        .presentation
        .len()
        .saturating_sub(MAX_PRESENTATION_PATCHES_PER_PASS);
    if deferred_presentation > 0 {
        report.warnings.push(format!(
            "deferred {deferred_presentation} presentation patches after bounded batch of {MAX_PRESENTATION_PATCHES_PER_PASS}"
        ));
    }

    report
        .adopted
        .extend(plan.adopt.iter().map(|s| s.identity.clone()));
    report.held.extend(plan.held.iter().cloned());
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
    presentation_cursor: &mut PresentationPatchCursor,
) -> UpReport {
    let _catalog_lock = match crate::CatalogLock::shared(root) {
        Ok(lock) => lock,
        Err(error) => {
            return UpReport {
                skipped: true,
                errors: vec![format!(
                    "acquire shared catalog-authoring lock (pass skipped): {error:#}"
                )],
                ..Default::default()
            };
        }
    };
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

    if let Err(error) = crate::reconcile::validate_task_identities(&found.specs, this_host) {
        report.errors.push(error.to_string());
        return report;
    }

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

    // Ordered, idempotent pre-boot materialization, with ownership checked against the complete
    // active fleet even when another gate defers one owner's writes. A gating render failure removes
    // only that agent from this pass; advisory git-exclude failures remain warnings and never block
    // a launch.
    let materialized = crate::materialize::materialize_catalog_against(
        root,
        &materializable_specs,
        &found.specs,
        this_host,
    );
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
    let mut plan = match crate::reconcile(&eligible_specs, &sessions, this_host) {
        Ok(plan) => plan,
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    report.deferred = debounce.defer_flickers(&mut plan, now);
    gate_codex_launches_on_hooks(&mut plan, root, &mut report, || match &hook_error {
        Some(error) => anyhow::bail!("{error}"),
        None => Ok(()),
    });
    execute_with_presentation_cursor(&plan, runner, cap, presentation_cursor, &mut report);
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
        &mut PresentationPatchCursor::default(),
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
    reconcile_pass_specs_with_cursor(
        specs,
        this_host,
        runner,
        cap,
        debounce,
        &mut PresentationPatchCursor::default(),
    )
}

pub(crate) fn reconcile_pass_specs_with_cursor(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
    presentation_cursor: &mut PresentationPatchCursor,
) -> UpReport {
    let mut report = UpReport::default();
    if let Err(error) = crate::reconcile::validate_task_identities(specs, this_host) {
        report.errors.push(error.to_string());
        return report;
    }
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
    reconcile_pass_specs_with_sessions(
        specs,
        &sessions,
        this_host,
        runner,
        cap,
        debounce,
        presentation_cursor,
    )
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
    presentation_cursor: &mut PresentationPatchCursor,
) -> UpReport {
    let mut report = UpReport::default();
    let now = Instant::now();
    debounce.observe(sessions, now);
    let mut plan = match crate::reconcile(specs, sessions, this_host) {
        Ok(plan) => plan,
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    report.deferred = debounce.defer_flickers(&mut plan, now);
    execute_with_presentation_cursor(&plan, runner, cap, presentation_cursor, &mut report);
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
    let _catalog_lock = crate::CatalogLock::shared(catalog_root)
        .context("acquire shared catalog-authoring lock for selected reconcile")?;
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
    if let Err(error) = crate::reconcile::validate_task_identities(&found.specs, this_host) {
        report.errors.push(error.to_string());
        return Ok(report);
    }
    let owner = owner.clone();
    if crate::hooks::required_by_codex_agent(&owner, this_host, catalog_root)
        && let Err(error) = crate::hooks::verify_installed()
    {
        report
            .errors
            .push(format!("verify lifecycle hooks: {error}"));
        return Ok(report);
    }
    let materialized = crate::materialize::materialize_catalog_against(
        catalog_root,
        std::slice::from_ref(&owner),
        &found.specs,
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
    crate::reconcile::validate_task_identities(specs, this_host)?;
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
    let mut presentation_cursor = PresentationPatchCursor::default();
    let mut reported_flapping: HashSet<String> = HashSet::new();
    loop {
        let report = reconcile_pass_specs_with_cursor(
            specs,
            this_host,
            runner,
            &mut cap,
            &mut debounce,
            &mut presentation_cursor,
        );
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
    let _catalog_lock = crate::CatalogLock::shared(root)
        .context("acquire shared catalog-authoring lock for teardown")?;
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
    let mut presentation_cursor = PresentationPatchCursor::default();

    // Surface each parked crash-loop once (not every pass): an stderr line AND a message to the
    // agent's supervisor over the native bus, so a crash-loop isn't only visible to whoever is
    // watching the log.
    let mut reported_flapping: HashSet<String> = HashSet::new();

    loop {
        let report = reconcile_pass(
            root,
            this_host,
            runner,
            &mut cap,
            &mut debounce,
            &mut presentation_cursor,
        );
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
    let Ok(Some(agent_dir)) = message::resolve_agent_dir(catalog_root, supervisor, this_host) else {
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
    use agent_spec::spec::{AgentSpec, JobType, Task, TaskKind, TaskLifecycle};
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsStr;

    fn target(id: &str, cmd: &str) -> TaskTarget {
        TaskTarget {
            kind: TaskKind::Pty,
            pty_id: id.to_string(),
            bus_id: "hetz.demo".to_string(),
            name: "agent".to_string(),
            derived: false,
            launch: TaskLaunch::Shell(cmd.to_string()),
            cwd: None,
            workspace: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            presentation: None,
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

    #[derive(Default)]
    struct PersistentPatchRunner {
        patched: RefCell<Vec<String>>,
    }

    impl Runner for PersistentPatchRunner {
        fn list_sessions(&self) -> anyhow::Result<Vec<Session>> {
            unreachable!("presentation execution does not list sessions")
        }

        fn spawn(&self, _target: &TaskTarget, _spec_dir: &Path) -> anyhow::Result<()> {
            unreachable!("presentation-only plan must not spawn")
        }

        fn kill(&self, _pty_id: &str) -> anyhow::Result<()> {
            unreachable!("presentation-only plan must not kill")
        }

        fn remove(&self, _pty_id: &str) -> anyhow::Result<()> {
            unreachable!("presentation-only plan must not remove")
        }

        fn patch_presentation(&self, presentation: &PtyPresentation) -> anyhow::Result<()> {
            self.patched.borrow_mut().push(presentation.pty_id.clone());
            if presentation.pty_id.as_str() < "host.presented.08" {
                anyhow::bail!("simulated persistent metadata failure");
            }
            Ok(())
        }
    }

    #[test]
    fn bounded_presentation_batches_are_deterministic_and_do_not_starve() {
        let plan = ReconcilePlan {
            presentation: (0..12)
                .rev()
                .map(|index| PtyPresentation {
                    pty_id: format!("host.presented.{index:02}"),
                    display_name: None,
                    tags: BTreeMap::new(),
                })
                .collect(),
            ..ReconcilePlan::default()
        };
        let runner = PersistentPatchRunner::default();
        let mut cap = FlappingCap::default();
        let mut cursor = PresentationPatchCursor::default();

        for _ in 0..2 {
            execute_with_presentation_cursor(
                &plan,
                &runner,
                &mut cap,
                &mut cursor,
                &mut UpReport::default(),
            );
        }

        let attempted = runner.patched.borrow();
        assert_eq!(
            &attempted[..8],
            &(0..8)
                .map(|index| format!("host.presented.{index:02}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(attempted.len(), 16);
        for index in 8..12 {
            assert!(attempted.contains(&format!("host.presented.{index:02}")));
        }
    }

    #[test]
    fn selected_codex_gate_suppresses_launch_on_stale_hooks() {
        let spec = AgentSpec {
            identity: "codex".into(),
            name: None,
            description: None,
            host: None,
            role: None,
            job_type: JobType::Service,
            workspace: None,
            supervisor: None,
            retired: false,
            keep: false,
            restart: None,
            resources: vec![],
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
                lifecycle: TaskLifecycle::Service,
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

    #[test]
    fn selected_identity_conflict_refuses_before_hook_verification_or_inventory() {
        let mut spec = AgentSpec {
            identity: "codex".into(),
            name: None,
            description: None,
            host: None,
            role: None,
            job_type: JobType::Service,
            workspace: None,
            supervisor: None,
            retired: false,
            keep: false,
            restart: None,
            resources: vec![],
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
                lifecycle: TaskLifecycle::Service,
            }],
            path: "/tmp/spec.kdl".into(),
        };
        spec.tasks[0]
            .env
            .insert("ST_AGENT".into(), "wrong.actor".into());
        let runner = GateRunner {
            list_calls: Cell::new(0),
        };
        let verify_calls = Cell::new(0);

        let error = up_once_selected_specs_with_gates(
            Path::new("/tmp"),
            &[spec],
            "test.codex.agent",
            "test",
            &runner,
            || {
                verify_calls.set(verify_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("conflicting ST_AGENT"));
        assert_eq!(verify_calls.get(), 0);
        assert_eq!(runner.list_calls.get(), 0);
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
            presentation: None,
        }
    }

    fn spec_fixture() -> AgentSpec {
        AgentSpec {
            identity: "demo".into(),
            name: None,
            description: None,
            host: Some("hetz".into()),
            role: None,
            job_type: JobType::Service,
            workspace: None,
            supervisor: None,
            retired: false,
            keep: false,
            restart: None,
            resources: vec![],
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
            live_derived: Vec::new(),
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
            live_derived: Vec::new(),
        });
        plan.launch.push(Launch {
            spec: &right,
            tasks: vec![right_agent],
            live_derived: Vec::new(),
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
            live_derived: Vec::new(),
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
            live_derived: Vec::new(),
        });
        plan.launch.push(Launch {
            spec: &claude,
            tasks: vec![claude_agent],
            live_derived: Vec::new(),
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
    /// and the established fallback presentation when no Agent Spec name is projected.
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
    fn build_run_command_projects_primary_name_and_owned_tags_at_spawn() {
        let key = "ST2_TEST_PRESENTATION_LITERAL_71c";
        unsafe { std::env::set_var(key, "expanded") }

        let cli = PtyCli::default();
        let mut t = target("hetz.demo", "codex");
        t.bus_id = "hetz.demo".to_owned();
        t.tags.insert("unrelated".to_owned(), "preserved".to_owned());
        t.presentation = Some(PtyPresentation {
            pty_id: "hetz.demo".to_owned(),
            display_name: Some(Some("Build owner".to_owned())),
            tags: BTreeMap::from([
                (
                    "agent.presentation.schema".to_owned(),
                    Some("1".to_owned()),
                ),
                (
                    "agent.actor.path".to_owned(),
                    Some("hetz.demo".to_owned()),
                ),
                (
                    "agent.presentation.description".to_owned(),
                    Some(format!("${key}")),
                ),
            ]),
        });
        let cmd = cli.build_run_command(&t, Path::new("/cat/hetz/demo"));
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        let name = args.iter().position(|arg| arg == "--name").unwrap();
        assert_eq!(args[name + 1], "Build owner");
        let tags = args
            .windows(2)
            .filter(|pair| pair[0] == "--tag")
            .map(|pair| pair[1].as_str())
            .collect::<BTreeSet<_>>();
        assert!(tags.contains("unrelated=preserved"));
        assert!(tags.contains("agent.presentation.schema=1"));
        assert!(tags.contains("agent.actor.path=hetz.demo"));
        assert!(tags.contains("agent.presentation.description=$ST2_TEST_PRESENTATION_LITERAL_71c"));
    }

    #[test]
    fn metadata_patch_uses_exact_id_and_one_json_stdin_payload() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("pty-capture");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\ncat > \"$0.stdin\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = PtyCli {
            bin: executable.display().to_string(),
            catalog_root: temporary.path().to_path_buf(),
        };
        let presentation = PtyPresentation {
            pty_id: "stable.agent.id".to_owned(),
            display_name: Some(None),
            tags: BTreeMap::from([
                (
                    "agent.presentation.schema".to_owned(),
                    Some("1".to_owned()),
                ),
                ("agent.presentation.description".to_owned(), None),
            ]),
        };

        cli.patch_presentation(&presentation).unwrap();

        assert_eq!(
            std::fs::read_to_string(executable.with_extension("args")).unwrap(),
            "metadata\npatch\n--id\nstable.agent.id\n"
        );
        let payload: serde_json::Value = serde_json::from_slice(
            &std::fs::read(executable.with_extension("stdin")).unwrap(),
        )
        .unwrap();
        assert_eq!(payload["displayName"], serde_json::Value::Null);
        assert_eq!(payload["tags"]["agent.presentation.schema"], "1");
        assert_eq!(
            payload["tags"]["agent.presentation.description"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn input_write_failure_terminates_and_reaps_the_child() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("close-stdin");
        let pidfile = temporary.path().join("child.pid");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$PIDFILE\"\nexec 0<&-\nsleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = vec![b'x'; 1024 * 1024];
        let error = output_with_input_timeout(
            Command::new(&executable).env("PIDFILE", &pidfile),
            Duration::from_secs(1),
            Some(input),
        )
        .unwrap_err();
        let pid = std::fs::read_to_string(pidfile)
            .unwrap()
            .parse::<i32>()
            .unwrap();

        assert!(
            format!("{error:#}").contains("Broken pipe"),
            "unexpected write error: {error:#}"
        );
        assert!(
            !crate::host_lock::process_alive(pid),
            "failed metadata child {pid} was not terminated and reaped"
        );
    }

    #[test]
    fn input_write_obeys_the_child_deadline() {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("ignore-stdin");
        let pidfile = temporary.path().join("child.pid");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$PIDFILE\"\nsleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = vec![b'x'; 1024 * 1024];
        let started = Instant::now();
        let error = output_with_input_timeout(
            Command::new(&executable).env("PIDFILE", &pidfile),
            Duration::from_millis(100),
            Some(input),
        )
        .unwrap_err();
        let pid = std::fs::read_to_string(pidfile)
            .unwrap()
            .parse::<i32>()
            .unwrap();

        assert!(
            format!("{error:#}").contains("timed out"),
            "unexpected write error: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "blocked stdin write ignored the child deadline"
        );
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while crate::host_lock::process_alive(pid) && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::host_lock::process_alive(pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn undrained_reader_does_not_retain_the_nonblocking_writer() {
        use std::os::fd::{FromRawFd as _, OwnedFd};

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let reader = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let pipe = std::fs::read_link(format!("/proc/self/fd/{}", reader.as_raw_fd())).unwrap();
        let started = Instant::now();

        assert!(
            !write_all_before(
                ChildStdin::from(writer),
                &vec![b'x'; 1024 * 1024],
                Instant::now() + Duration::from_millis(100),
            )
            .unwrap()
        );
        let retained_writers = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target == &pipe)
            .count();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            retained_writers, 1,
            "the undrained pipe retained a writer after the deadline"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn expired_write_deadline_prevents_further_progress() {
        use std::os::fd::{FromRawFd as _, OwnedFd};

        let mut pipe_fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let _reader = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let writer = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

        assert!(
            !write_all_before(ChildStdin::from(writer), b"x", Instant::now()).unwrap(),
            "an expired child deadline still allowed stdin progress"
        );
    }

    #[test]
    fn expired_cleanup_deadline_hands_reaping_off_without_blocking() {
        let child = Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id() as i32;
        let started = Instant::now();
        terminate_and_reap_before(child, pid, Instant::now());

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "expired cleanup deadline blocked the caller"
        );
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while crate::host_lock::process_alive(pid) && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !crate::host_lock::process_alive(pid),
            "background reaper did not collect child {pid}"
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
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
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
        t.presentation = Some(PtyPresentation {
            pty_id: t.pty_id.clone(),
            display_name: Some(Some(t.pty_id.clone())),
            tags: BTreeMap::new(),
        });
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
    fn managed_agent_scrubs_ambient_no_color_unless_explicitly_declared() {
        let cli = PtyCli::default();
        let agent = target("hetz.demo.agent", "exec claude 'boot'");
        let command = cli.build_run_command(&agent, Path::new("/cat/hetz/demo"));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new("NO_COLOR"))
                .map(|(_, value)| value),
            Some(None),
            "ambient NO_COLOR must not silently disable an interactive agent's color"
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
            "the removal must be persisted for PTY restart"
        );

        let mut explicit = target("hetz.explicit.agent", "exec claude 'boot'");
        explicit.env.insert("NO_COLOR".into(), "1".into());
        let command = cli.build_run_command(&explicit, Path::new("/cat/hetz/explicit"));
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new("NO_COLOR"))
                .and_then(|(_, value)| value),
            Some(OsStr::new("1")),
            "an explicit Agent Spec NO_COLOR remains authoritative"
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !args
                .windows(2)
                .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
            "an explicit assignment must not also persist a removal"
        );
    }

    #[test]
    fn non_agent_task_does_not_claim_no_color_policy() {
        let cli = PtyCli::default();
        let mut task = target("hetz.demo.sidecar", "exec sleep 1");
        task.name = "sidecar".into();
        let command = cli.build_run_command(&task, Path::new("/cat/hetz/demo"));

        assert!(
            command
                .get_envs()
                .all(|(key, _)| key != OsStr::new("NO_COLOR")),
            "non-agent services keep the caller's ambient NO_COLOR semantics"
        );
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !args
                .windows(2)
                .any(|pair| pair == ["--unset-env", "NO_COLOR"]),
            "non-agent services must not persist an st2-owned removal"
        );
    }

    #[test]
    fn isolation_wrapper_preserves_environment_removals() {
        let mut inner = Command::new("pty");
        inner.env("TERM", "xterm-256color").env_remove("NO_COLOR");
        let mut outer = Command::new("systemd-run");

        apply_command_env(&inner, &mut outer);

        let env = outer
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            env.get(OsStr::new("TERM")).and_then(Option::as_deref),
            Some(OsStr::new("xterm-256color"))
        );
        assert_eq!(env.get(OsStr::new("NO_COLOR")), Some(&None));
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

    #[test]
    fn task_observation_of_missing_pty_root_is_complete_and_does_not_create_it() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        std::fs::create_dir(&catalog).unwrap();
        std::fs::write(
            catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                tmp.path().join("missing-pty").display().to_string()
            ),
        )
        .unwrap();
        let root = effective_pty_root_from(&catalog, None);
        assert!(!root.exists());
        let batch = PtyCli::new(catalog).task_observations(&HashSet::from(["h.worker"]));
        assert!(batch.complete, "{:?}", batch.errors);
        assert!(batch.observations.is_empty());
        assert!(!root.exists(), "read-only observation created the PTY root");
    }

    #[test]
    fn unreadable_pty_root_evidence_is_indeterminate_not_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let loop_path = tmp.path().join("pty-loop");
        std::fs::create_dir(&catalog).unwrap();
        std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
        std::fs::write(
            catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                loop_path.display().to_string()
            ),
        )
        .unwrap();
        let batch = PtyCli::new(catalog)
            .task_observations_at_root(&HashSet::from(["h.worker"]), &loop_path);
        assert!(!batch.complete);
        assert!(batch.observations.is_empty());
        assert!(
            batch.errors[0].contains("cannot inspect PTY root"),
            "{:?}",
            batch.errors
        );
    }

    #[test]
    fn removed_and_recreated_pty_root_is_indeterminate_not_absent() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pty");
        std::fs::create_dir(&root).unwrap();
        let fake = tmp.path().join("pty-bin");
        std::fs::write(
            &fake,
            "#!/bin/sh\nrmdir \"$PTY_ROOT\"\nmkdir \"$PTY_ROOT\"\nprintf '%s\\n' '[]'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();

        let batch = PtyCli {
            bin: fake.display().to_string(),
            catalog_root: tmp.path().join("catalog"),
        }
        .task_observations_at_root(&HashSet::from(["h.worker"]), &root);
        assert!(!batch.complete);
        assert!(batch.observations.is_empty());
        assert!(
            batch.errors[0].contains("changed identity during observation"),
            "{:?}",
            batch.errors
        );
    }

    #[test]
    fn pty_task_observation_preserves_exact_generation_and_closed_states() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let pty_root = tmp.path().join("pty");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::create_dir(&pty_root).unwrap();
        std::fs::write(
            catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                pty_root.display().to_string()
            ),
        )
        .unwrap();
        let fake = tmp.path().join("pty-bin");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' '[{"name":"h.live","status":"running","pid":41,"createdAt":"2026-07-31T10:00:00.000Z","displayName":"Build owner","tags":{"agent.presentation.schema":"1","unrelated":"preserved"}},{"name":"h.exit","status":"exited","exitCode":0,"pid":42,"createdAt":"2026-07-31T09:00:00.000Z"},{"name":"h.gone","status":"vanished","pid":43,"createdAt":"2026-07-31T08:00:00.000Z"}]'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();

        let cli = PtyCli {
            bin: fake.display().to_string(),
            catalog_root: catalog,
        };
        let desired = HashSet::from(["h.live", "h.exit", "h.gone"]);
        let first = cli.task_observations(&desired);
        let second = cli.task_observations(&desired);
        assert!(first.complete, "{:?}", first.errors);
        assert_eq!(first, second, "same PTY evidence changed generation");
        let ObservedState::Running(generation) = &first.observations[0].state else {
            panic!("running PTY lost generation: {:?}", first.observations[0]);
        };
        assert_eq!(generation.pid(), 41);
        assert_eq!(generation.created_at(), "2026-07-31T10:00:00.000Z");
        assert!(generation.generation_id().starts_with("sha256:"));
        assert_eq!(first.observations[1].state, ObservedState::Exited);
        assert_eq!(first.observations[2].state, ObservedState::Vanished);

        let sessions = cli.list_sessions().unwrap();
        let presentation = sessions[0].presentation.as_ref().unwrap();
        assert_eq!(presentation.display_name.as_deref(), Some("Build owner"));
        assert_eq!(
            presentation.tags.get("agent.presentation.schema").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            presentation.tags.get("unrelated").map(String::as_str),
            Some("preserved")
        );
    }

    #[test]
    fn running_pty_without_complete_generation_is_indeterminate() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        let pty_root = tmp.path().join("pty");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::create_dir(&pty_root).unwrap();
        std::fs::write(
            catalog.join("catalog.kdl"),
            format!(
                "catalog {{ pty-root {:?} }}\n",
                pty_root.display().to_string()
            ),
        )
        .unwrap();
        let fake = tmp.path().join("pty-bin");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' '[{\"name\":\"h.live\",\"status\":\"running\"}]'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        let batch = PtyCli {
            bin: fake.display().to_string(),
            catalog_root: catalog,
        }
        .task_observations(&HashSet::from(["h.live"]));
        assert!(!batch.complete);
        assert!(matches!(
            batch.observations[0].state,
            ObservedState::Indeterminate(_)
        ));
    }
}
