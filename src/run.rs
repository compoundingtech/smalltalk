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
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use opentelemetry::trace::Status;
use serde::{Deserialize, Serialize};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::exec_backend::ExecBackend;
use crate::flapping::FlappingCap;
use crate::message;
use crate::reconcile::{
    PtyPresentation, ReconcilePlan, Session, TaskCompileContext, TaskLaunch, TaskTarget,
    compile_generated_tasks,
};
use crate::task_inventory::{
    DesiredRuntime, ObservationBatch, ObservedState, ResourceTarget,
    ResourceTargetUnavailableReason, RuntimeGeneration, RuntimeObservation, RuntimeObserver,
    generation_id, observe_resource_target,
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

mod process;
pub(crate) use process::*;

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
    #[cfg(test)]
    on_command_spawn: Option<std::sync::Arc<dyn Fn(i32)>>,
}

impl Default for PtyCli {
    fn default() -> Self {
        Self {
            bin: "pty".to_string(),
            catalog_root: PathBuf::from("."),
            #[cfg(test)]
            on_command_spawn: None,
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

/// One live session returned by the socket-backed `pty stats --json` snapshot.
#[derive(Debug, Deserialize)]
struct PtyStatsEntry {
    name: String,
    #[serde(default)]
    process: Option<PtyStatsProcess>,
    #[serde(default)]
    daemon: Option<PtyStatsDaemon>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PtyStatsProcess {
    alive: bool,
}

#[derive(Debug, Deserialize)]
struct PtyStatsDaemon {
    pid: u32,
}

fn confirm_pty_generation(
    initial: &PtyListEntry,
    stats: &[PtyStatsEntry],
) -> Result<(), ResourceTargetUnavailableReason> {
    let mut matching = stats.iter().filter(|candidate| candidate.name == initial.name);
    let Some(current) = matching.next() else {
        return Err(ResourceTargetUnavailableReason::ProcessUnavailable);
    };
    if matching.next().is_some() {
        return Err(ResourceTargetUnavailableReason::RuntimeIndeterminate);
    }
    let (Some(process), Some(daemon), Some(created_at)) =
        (&current.process, &current.daemon, &current.created_at)
    else {
        return Err(ResourceTargetUnavailableReason::RuntimeIndeterminate);
    };
    if !process.alive {
        return Err(ResourceTargetUnavailableReason::ProcessUnavailable);
    }
    if Some(daemon.pid) != initial.pid || Some(created_at) != initial.created_at.as_ref() {
        return Err(ResourceTargetUnavailableReason::GenerationChanged);
    }
    Ok(())
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

/// The portable bound on a unix socket address. Darwin caps `sun_path` at 104 bytes and Linux at
/// 108, so a declaration has to satisfy the smaller one to be admissible on either.
pub const PORTABLE_SOCKET_PATH_LIMIT: usize = 104;

/// The socket `pty` binds for one session: `<PTY_ROOT>/<session-id>.sock`.
///
/// Measured against the pty binary rather than assumed: for a 21-byte root and a 78-byte id it
/// reports a 105-byte path exceeding the limit by 1, which is `root + '/' + id + ".sock"`.
pub fn session_socket_path(pty_root: &Path, session_id: &str) -> PathBuf {
    pty_root.join(format!("{session_id}.sock"))
}

/// The resolved socket path and its overage, when a task's session socket cannot be bound.
///
/// `pty` refuses the bind rather than truncating, so such a task can never spawn: it fails
/// identically on every reconcile pass, forever. The bound is not a constant — the usable identity
/// length is what remains of the limit after the resolved pty root — so it is always derived from
/// the root actually in use.
pub fn session_socket_overage(pty_root: &Path, session_id: &str) -> Option<(PathBuf, usize)> {
    let path = session_socket_path(pty_root, session_id);
    let bytes = path.as_os_str().as_encoded_bytes().len();
    bytes
        .checked_sub(PORTABLE_SOCKET_PATH_LIMIT)
        .filter(|over| *over > 0)
        .map(|over| (path, over))
}

impl PtyCli {
    /// A `PtyCli` rooted at `catalog_root` (used for `$CATALOG` expansion).
    pub fn new(catalog_root: PathBuf) -> Self {
        Self {
            bin: "pty".to_string(),
            catalog_root,
            #[cfg(test)]
            on_command_spawn: None,
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

    /// Expand direct arguments against the environment that the managed task receives. The task
    /// overlay wins over the launcher environment, as it does after the process starts.
    fn expand_managed(&self, value: &str, managed_env: &BTreeMap<OsString, OsString>) -> String {
        crate::expand::expand_vars(value, |key| {
            managed_env
                .get(OsStr::new(key))
                .map(|value| value.to_string_lossy().into_owned())
                .or_else(|| std::env::var(key).ok())
        })
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
                cmd.args(
                    argv.iter()
                        .map(|arg| self.expand_managed(arg, &managed_env)),
                );
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
        // `pty list` identifies the daemon generation but is registry state,
        // not a live socket proof. Capture each candidate daemon's kernel start
        // token, query all live session sockets once, then accept a token only
        // when stats reports the same name, daemon PID, and createdAt and a
        // second token read is unchanged.
        let start_tokens = entries
            .iter()
            .filter(|entry| {
                desired_ids.contains(entry.name.as_str())
                    && entry.status == "running"
                    && entry.pid.is_some()
                    && entry.created_at.is_some()
            })
            .map(|entry| {
                (
                    entry.name.clone(),
                    crate::exec_backend::process_start_time_ticks(entry.pid.unwrap() as i32).ok(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let stats_entries = start_tokens
            .values()
            .any(Option::is_some)
            .then(|| self.stats_entries_at(root));
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
                        let start_time_ticks = start_tokens.get(&entry.name).copied().flatten();
                        let resource_target = match (start_time_ticks, stats_entries.as_ref()) {
                            (Some(start_time_ticks), Some(Ok(stats))) => {
                                match confirm_pty_generation(&entry, stats) {
                                    Ok(()) => {
                                        let final_start =
                                            crate::exec_backend::process_start_time_ticks(
                                                pid as i32,
                                            );
                                        match final_start {
                                            Ok(final_start) if final_start == start_time_ticks => {
                                                observe_resource_target(
                                                    pid,
                                                    Some(start_time_ticks),
                                                )
                                            }
                                            Ok(_) => ResourceTarget::unavailable(
                                                ResourceTargetUnavailableReason::GenerationChanged,
                                            ),
                                            Err(_) => ResourceTarget::unavailable(
                                                ResourceTargetUnavailableReason::ProcessUnavailable,
                                            ),
                                        }
                                    }
                                    Err(reason) => ResourceTarget::unavailable(reason),
                                }
                            }
                            (Some(_), Some(Err(_))) => ResourceTarget::unavailable(
                                ResourceTargetUnavailableReason::RuntimeIndeterminate,
                            ),
                            _ => ResourceTarget::unavailable(
                                ResourceTargetUnavailableReason::ProcessUnavailable,
                            ),
                        };
                        // Transient resource proof must not participate in
                        // stable PTY generation identity.
                        let generation_id =
                            generation_id("pty", &entry.name, pid, created_at, None);
                        match RuntimeGeneration::new(
                            pid,
                            created_at.to_owned(),
                            generation_id,
                            resource_target,
                        ) {
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
        let mut command = Command::new(&self.bin);
        command
            .args(["metadata", "patch", "--id", &presentation.pty_id])
            .env("PTY_ROOT", effective_pty_root(&self.catalog_root));
        #[cfg(test)]
        let out = output_with_input_timeout_observed(
            &mut command,
            PTY_LIST_TIMEOUT,
            Some(payload),
            |pid| {
                if let Some(on_spawn) = &self.on_command_spawn {
                    on_spawn(pid);
                }
            },
        );
        #[cfg(not(test))]
        let out = output_with_input_timeout(&mut command, PTY_LIST_TIMEOUT, Some(payload));
        let out =
            out.map_err(|error| anyhow::anyhow!("`pty metadata patch --id` failed: {error}"))?;
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
        #[cfg(test)]
        let out = output_full_stdout_with_timeout_observed(
            Command::new(&self.bin)
                .args(["list", "--json"])
                .env("PTY_ROOT", root),
            PTY_LIST_TIMEOUT,
            |pid| {
                if let Some(on_spawn) = &self.on_command_spawn {
                    on_spawn(pid);
                }
            },
        );
        #[cfg(not(test))]
        let out = output_full_stdout_with_timeout(
            Command::new(&self.bin)
                .args(["list", "--json"])
                .env("PTY_ROOT", root),
            PTY_LIST_TIMEOUT,
        );
        let out = out.map_err(|error| anyhow::anyhow!("`pty list --json` failed: {error}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty list --json` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|error| anyhow::anyhow!("parsing `pty list --json`: {error}"))
    }

    fn stats_entries_at(&self, root: &Path) -> anyhow::Result<Vec<PtyStatsEntry>> {
        let out = output_full_stdout_with_timeout(
            Command::new(&self.bin)
                .args(["stats", "--json"])
                .env("PTY_ROOT", root),
            PTY_LIST_TIMEOUT,
        )
        .map_err(|error| anyhow::anyhow!("`pty stats --json` failed: {error}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "`pty stats --json` failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|error| anyhow::anyhow!("parsing `pty stats --json`: {error}"))
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

    /// Fully retire a launched stream runtime before its declaration is removed.
    pub fn retire(&self, runtime_id: &str) -> anyhow::Result<()> {
        match self.index.borrow().get(runtime_id) {
            Some(TaskKind::Exec) => self.exec.retire(runtime_id),
            Some(TaskKind::Pty) => {
                self.pty.kill(runtime_id)?;
                self.pty.reap_for_restart(runtime_id)
            }
            None => anyhow::bail!("runtime '{runtime_id}' disappeared before retirement"),
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
                    start_time_ticks,
                })) => {
                    let state = match RuntimeGeneration::new(
                        pid,
                        created_at,
                        generation_id,
                        observe_resource_target(pid, Some(start_time_ticks)),
                    ) {
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

/// The machine-local state root shared by host runtime state and supervisor-scoped channels.
pub(crate) fn state_root() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The machine-local runner-state dir for a host's exec tasks: `$XDG_STATE_HOME/st2/<host>/exec`
/// (falling back to `~/.local/state`). Not synced — pids are host-local.
pub fn exec_state_dir(host: &str) -> PathBuf {
    state_root().join("st2").join(host).join("exec")
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
    /// pty ids released from a park this pass by an explicit operator request.
    pub unparked: Vec<String>,
    /// Rich crash-loop records (a superset of `flapping`) — the source for supervisor surfacing.
    pub crash_loops: Vec<CrashLoop>,
    /// identities adopted (already fully present).
    pub adopted: Vec<String>,
    /// identities skipped as belonging to another host.
    pub other_host: Vec<String>,
    /// identities with no runnable task (unrendered).
    pub unrunnable: Vec<String>,
    /// bus ids archived out of the live catalog this pass because their retirement outlived
    /// `archive-after`.
    pub archived: Vec<String>,
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
        self.unparked.append(&mut other.unparked);
        self.crash_loops.append(&mut other.crash_loops);
        self.adopted.append(&mut other.adopted);
        self.other_host.append(&mut other.other_host);
        self.unrunnable.append(&mut other.unrunnable);
        self.archived.append(&mut other.archived);
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
            || !self.unparked.is_empty()
            || !self.archived.is_empty()
            || !self.warnings.is_empty()
            || !self.errors.is_empty()
    }
}

/// Why a task is parked, in the operator's terms. Published with every marker so the fault reads on
/// its own, without the reader having to already know what `mode = fail` means.
pub const PARK_REASON: &str = "crash-looped past its restart{} policy (mode=fail)";

/// Grant the operator's pending unpark requests into `cap`.
///
/// Call this *before* the pass plans anything, so a granted recovery relaunches in the same pass that
/// granted it. Deferring it to the next pass would make the operator's targeted act take up to a full
/// `--interval` to show any effect, which reads as "it did nothing" and invites a second attempt.
///
/// Only the long-lived supervisor loops may call it. A one-shot `up_once` builds a fresh
/// [`FlappingCap`] with an empty parked set, so consuming a request there would silently discard it.
pub fn grant_unpark_requests(cap: &mut FlappingCap, request_dir: &Path, report: &mut UpReport) {
    let (ids, errors) = crate::park::take_unpark_requests(request_dir);
    report.errors.extend(errors);
    for id in ids {
        if cap.unpark(&id) {
            report.unparked.push(id);
        } else {
            report
                .warnings
                .push(format!("unpark '{id}': not parked; nothing to recover"));
        }
    }
}

/// Republish the parked projection to match `cap`.
///
/// Call this *after* the pass has executed, so a task parked by this very pass is already visible to
/// the next `st2 tasks`. Republishing also retracts the marker of anything no longer parked, so a
/// recovered task stops reporting a fault without anyone remembering to clean up.
///
/// Supervisor loops only, for the same reason as [`grant_unpark_requests`]: publishing an empty
/// one-shot cap would wipe the running supervisor's projection and hide every live park.
pub fn publish_parks(
    cap: &FlappingCap,
    projection: &crate::park::ParkProjection,
    report: &mut UpReport,
) {
    let parked: std::collections::BTreeSet<String> = cap.parked_ids().cloned().collect();
    report
        .errors
        .extend(projection.publish(&parked, PARK_REASON));
}

/// A supervisor loop's end of the park channel: the projection it publishes and the request dir it
/// drains. Bundled because the two are only ever used together, and only by a loop that owns a
/// long-lived [`FlappingCap`].
///
/// A supervisor that cannot identify its own process generation cannot write a believable marker, so
/// it publishes nothing rather than something a reader would have to guess about. Parking itself is
/// unaffected — it degrades to exactly the pre-#204 behaviour, loudly.
pub struct ParkChannel {
    projection: Option<crate::park::ParkProjection>,
    request_dir: Option<PathBuf>,
}

impl ParkChannel {
    pub fn for_supervisor(catalog_root: &Path, host: &str) -> Self {
        let scope = match crate::park::SupervisorScope::current(catalog_root, host) {
            Ok(scope) => scope,
            Err(error) => {
                tracing::warn!(
                    "st2: cannot open the supervisor park channel ({error}); parks remain terminal but cannot be observed or explicitly released."
                );
                return Self {
                    projection: None,
                    request_dir: None,
                };
            }
        };
        let projection = match crate::park::ParkProjection::current(scope.park_dir()) {
            Ok(projection) => Some(projection),
            Err(error) => {
                tracing::warn!(
                    "st2: cannot publish parked tasks ({error}); `st2 tasks` will not show park faults for this supervisor."
                );
                None
            }
        };
        Self {
            projection,
            request_dir: Some(scope.unpark_request_dir()),
        }
    }

    fn grant_requests(&self, cap: &mut FlappingCap, report: &mut UpReport) {
        if let Some(request_dir) = &self.request_dir {
            grant_unpark_requests(cap, request_dir, report);
        }
    }

    fn publish(&self, cap: &FlappingCap, report: &mut UpReport) {
        if let Some(projection) = &self.projection {
            publish_parks(cap, projection, report);
        }
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
        &mut |_| {},
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
            Err(error) => report.errors.push(format!(
                "kill unavailable derived companion {companion_id}: {error}"
            )),
        }
    }
}

/// Bounded driver label for lifecycle metrics (`task_launches_total` / `task_reaps_total`).
/// Typed drivers report their own name; legacy routed and hand-authored seats are classified
/// by what their launch actually invokes. Anything unrecognizable collapses to `other`, so
/// the label stays a closed set: `codex|claude|opencode|pi|omp|exec|other`. Observational only —
/// callers gate on [`crate::metrics::enabled`], and it never influences reconcile decisions.
fn driver_label(launch: &crate::reconcile::Launch<'_>, target: &TaskTarget) -> &'static str {
    if target.kind == TaskKind::Exec {
        return "exec";
    }
    if let Some(driver) = &launch.spec.driver {
        return driver.name();
    }
    // Legacy routed (`deliver "mcp"` → claude-session, ...) or hand-authored seats: inspect
    // the launch source by its alphanumeric tokens.
    let tokens = |needle: &str| match &target.launch {
        TaskLaunch::Argv(argv) => argv.iter().any(|arg| {
            arg.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|t| t == needle)
        }),
        TaskLaunch::Shell(command) => command
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == needle),
    };
    if tokens("codex") {
        "codex"
    } else if tokens("claude") {
        "claude"
    } else if tokens("opencode") {
        "opencode"
    } else if tokens("omp") {
        "omp"
    } else if tokens("pi") {
        "pi"
    } else {
        "other"
    }
}

fn execute_with_presentation_cursor(
    plan: &ReconcilePlan,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    presentation_cursor: &mut PresentationPatchCursor,
    report: &mut UpReport,
    on_canonical_live: &mut dyn FnMut(&agent_spec::spec::AgentSpec),
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
                    Ok(()) => {
                        crate::metrics::record_task_reap(driver_label(launch, target));
                    }
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
            let spawn_started = Instant::now();
            match runner.spawn(target, spec_dir) {
                Ok(()) => {
                    crate::metrics::record_session_start(
                        spawn_started.elapsed(),
                        driver_label(launch, target),
                    );
                    cap.record(&target.pty_id, now);
                    if restarting {
                        report.restarted.push(target.pty_id.clone());
                    } else {
                        report.launched.push(target.pty_id.clone());
                    }
                    if target.name == "agent" && !target.derived {
                        agent_available = true;
                        // Baseline the canonical seat synchronously at the exact transition that
                        // made it live. A later target may block while its carriers keep changing.
                        on_canonical_live(launch.spec);
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

    // Uptime is what forgives a `mode = fail` budget, so every pass is closed, not only the ones
    // that launched something. The cap is told what the pass PROVED alive (`plan.live`) rather than
    // being left to infer it from what the pass did not launch: this plan may have been narrowed
    // after reconcile (hook gating, flicker debouncing) or built from a reduced spec set (an owner
    // that failed to materialize), and a task dropped that way is unobserved, not healthy. A pass
    // that bailed before `execute` (lock failure, skipped) never gets here and credits nothing —
    // the same safe direction.
    cap.end_pass(Instant::now(), &plan.live);

    let mut failed_retirement_teardowns = HashSet::new();
    for td in &plan.teardown {
        let mut failed = false;
        for id in &td.pty_ids {
            match runner.kill(id) {
                Ok(()) => report.torn_down.push(id.clone()),
                Err(e) => {
                    failed = true;
                    report.errors.push(format!("kill {id}: {e}"));
                }
            }
        }
        if failed {
            failed_retirement_teardowns.insert(td.spec.path.clone());
        }
    }
    for spec in &plan.settle_retirement {
        if failed_retirement_teardowns.contains(&spec.path) {
            continue;
        }
        let agent_dir = spec.path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(error) = crate::message::archive_inbox(agent_dir) {
            report.errors.push(format!(
                "archive retired inbox for {}: {error:#}",
                spec.identity
            ));
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

/// Specs whose canonical agent seat this pass proved live AND whose desired state is `running`.
/// Positive liveness is never inferred from desired state or whole-spec adoption — the canonical
/// task itself must have been observed alive or spawned successfully. But a non-running desired
/// state is a NEGATIVE gate: a suspended or retired agent owns no live subscription work even while
/// its seat is still alive mid-teardown, so its resync installs and resource-Profile subscriptions
/// are stripped this pass rather than lingering until the seat dies (dotfiles#1535). The declaration
/// and its `resources/` are untouched; only the runtime work stops.
fn live_resync_specs(
    specs: &[agent_spec::spec::AgentSpec],
    this_host: &str,
    sessions: &[Session],
    report: &UpReport,
) -> Vec<agent_spec::spec::AgentSpec> {
    let live_task_ids = sessions
        .iter()
        .filter(|session| session.alive)
        .map(|session| session.pty_id.as_str())
        .chain(report.launched.iter().map(String::as_str))
        .chain(report.restarted.iter().map(String::as_str))
        .collect::<HashSet<_>>();
    specs
        .iter()
        .filter(|spec| {
            if !spec.desired_state.is_running() {
                return false;
            }
            spec.tasks.iter().any(|task| {
                if task.name != "agent" {
                    return false;
                }
                let task_id = task
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("{}.{}", spec.bus_id(this_host), task.name));
                live_task_ids.contains(task_id.as_str())
            })
        })
        .cloned()
        .collect()
}

/// One full reconcile pass: discover → list actual → reconcile → execute. On a `pty list` failure the
/// pass is SKIPPED (the error is recorded but nothing is reconciled) — treating a transient list
/// failure as "no sessions" would double-spawn everything. `cap` carries flapping state across passes;
/// `debounce` carries per-id liveness so a transient not-alive flicker isn't destructively reaped.
fn reconcile_pass(
    root: &Path,
    this_host: &str,
    task_context: &TaskCompileContext,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
    presentation_cursor: &mut PresentationPatchCursor,
    resync: Option<&crate::resync::ResyncSupervisor>,
    resource_profiles: Option<&crate::resource_profile_supervisor::ResourceProfileSupervisor>,
) -> UpReport {
    let catalog_lock = {
        let span = catalog_lock_span();
        let entered = span.as_ref().map(tracing::Span::enter);
        let result = crate::CatalogLock::shared(root);
        finish_child_span(span.as_ref(), result.is_err());
        drop(entered);
        match result {
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
        }
    };
    let found = {
        let span = catalog_discover_span();
        let entered = span.as_ref().map(tracing::Span::enter);
        let found = crate::discover(root);
        if let Some(span) = &span {
            span.record("st2.catalog.spec_count", span_count(found.specs.len()));
            span.record("st2.report.warning_count", span_count(found.warnings.len()));
            span.record("st2.report.error_count", span_count(found.errors.len()));
            finish_child_span(Some(span), !found.errors.is_empty());
        }
        drop(entered);
        found
    };
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
    //
    // pi needs the same verified set, but for its launch only: nothing it renders references
    // `$ST_HOOKS`, while `st2 driver pi-session` cannot start without the channel extension. So a
    // pi agent contributes to this verification without having its materialization deferred.
    let tracer_export_enabled = crate::telemetry::tracer_export_enabled();
    let needs_codex_hooks = crate::hooks::required_by_codex(&found.specs, this_host, root);
    let needs_pi_hooks = if needs_codex_hooks && !tracer_export_enabled {
        false
    } else {
        crate::hooks::required_by_pi(&found.specs, this_host, root)
    };
    let needs_omp_hooks = if (needs_codex_hooks || needs_pi_hooks) && !tracer_export_enabled {
        false
    } else {
        crate::hooks::required_by_omp(&found.specs, this_host, root)
    };
    let needs_hooks = needs_codex_hooks || needs_pi_hooks || needs_omp_hooks;
    let hook_error = {
        let consumer = match (needs_codex_hooks, needs_pi_hooks, needs_omp_hooks) {
            (true, true, true) => "codex+pi+omp",
            (true, true, false) => "codex+pi",
            (true, false, true) => "codex+omp",
            (false, true, true) => "pi+omp",
            (true, false, false) => "codex",
            (false, true, false) => "pi",
            (false, false, true) => "omp",
            (false, false, false) => "none",
        };
        let span = needs_hooks
            .then(|| lifecycle_hooks_span(consumer))
            .flatten();
        let entered = span.as_ref().map(tracing::Span::enter);
        let hook_error = needs_hooks
            .then(crate::hooks::verify_required_set)
            .transpose()
            .err()
            .map(|error| error.to_string());
        finish_child_span(span.as_ref(), hook_error.is_some());
        drop(entered);
        hook_error
    };
    if let Some(error) = &hook_error {
        report.errors.push(format!(
            "verify this binary's lifecycle hooks before harness materialization: {error}; materialization deferred"
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
    let materialized = {
        let span = catalog_materialize_span("catalog");
        let entered = span.as_ref().map(tracing::Span::enter);
        let materialized = crate::materialize::materialize_catalog_against(
            root,
            &materializable_specs,
            &found.specs,
            this_host,
        );
        if let Some(span) = &span {
            span.record(
                "st2.materialize.failure_count",
                span_count(materialized.failed_agents.len()),
            );
            span.record(
                "st2.report.warning_count",
                span_count(materialized.warnings.len()),
            );
            span.record(
                "st2.report.error_count",
                span_count(materialized.errors.len()),
            );
            finish_child_span(Some(span), !materialized.errors.is_empty());
        }
        drop(entered);
        materialized
    };
    report.warnings.extend(materialized.warnings);
    report.errors.extend(materialized.errors);
    let mut compiled_specs = Vec::new();
    for mut spec in found.specs.iter().cloned() {
        if let Err(error) =
            compile_generated_tasks(std::slice::from_mut(&mut spec), this_host, task_context)
        {
            report.errors.push(format!(
                "compile generated tasks for {}: {error:#}",
                spec.path.display()
            ));
            continue;
        }
        compiled_specs.push(spec);
    }
    let eligible_specs = compiled_specs
        .iter()
        .filter(|spec| !materialized.failed_agents.contains(&spec.bus_id(this_host)))
        .cloned()
        .collect::<Vec<_>>();

    let sessions = {
        let span = runtime_observe_span();
        let entered = span.as_ref().map(tracing::Span::enter);
        let sessions = runner.list_sessions();
        if let (Some(span), Ok(sessions)) = (span.as_ref(), sessions.as_ref()) {
            span.record("st2.runtime.session_count", span_count(sessions.len()));
        }
        finish_child_span(span.as_ref(), sessions.is_err());
        drop(entered);
        match sessions {
            Ok(sessions) => sessions,
            Err(e) => {
                report.skipped = true;
                report
                    .errors
                    .push(format!("list sessions (pass skipped): {e}"));
                return report;
            }
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
    if let Some(resync) = resync {
        for launch in &plan.launch {
            if launch.tasks.iter().any(|task| task.name == "agent") {
                resync.deactivate(launch.spec, this_host);
            }
        }
    }
    if let Some(resource_profiles) = resource_profiles {
        for launch in &plan.launch {
            if launch.tasks.iter().any(|task| task.name == "agent") {
                resource_profiles.deactivate(launch.spec);
            }
        }
    }
    gate_harness_launches_on_hooks(&mut plan, root, &mut report, |_| match &hook_error {
        Some(error) => anyhow::bail!("{error}"),
        None => Ok(()),
    });
    if let Some(resync) = resync {
        // Existing canonical seats are established by this pass's observation. Reinstall their
        // complete catalog-aware sets synchronously before unrelated repairs can block. The
        // targeted upsert retains unchanged baselines and pending transitions, so this is
        // idempotent across steady-state passes.
        for spec in live_resync_specs(&compiled_specs, this_host, &sessions, &report) {
            report
                .warnings
                .extend(resync.install_live(&spec, &found.specs, this_host));
        }
    }
    let mut boundary_warnings = Vec::new();
    let mut install_new_live_seat = |spec: &agent_spec::spec::AgentSpec| {
        if let Some(resync) = resync {
            boundary_warnings.extend(resync.install_live(spec, &found.specs, this_host));
        }
    };
    execute_reconcile(
        &plan,
        runner,
        cap,
        presentation_cursor,
        &mut report,
        &mut install_new_live_seat,
    );
    report.warnings.extend(boundary_warnings);
    if resync.is_some() || resource_profiles.is_some() {
        let loaded = crate::catalog::declared_profile_catalog(root)
            .context("parse resource profiles in catalog.kdl");
        let catalog_profile_error = loaded.is_err();
        let malformed_declarations = found
            .errors
            .iter()
            .map(|error| error.path.clone())
            .filter(|path| {
                !catalog_profile_error || *path != crate::catalog::config_path(root)
            })
            .collect::<Vec<_>>();
        let (config, profiles) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                report.errors.push(format!("{error:#}"));
                (
                    crate::catalog::CatalogConfig::default(),
                    agent_spec::profile::ResourceProfileRegistry::empty(),
                )
            }
        };
        let live_subscription_specs =
            live_resync_specs(&compiled_specs, this_host, &sessions, &report);
        if let Some(resource_profiles) = resource_profiles {
            let generation = match crate::catalog_lock::read_generation_token(root) {
                Ok(generation) => generation,
                Err(error) => {
                    report.errors.push(format!(
                        "read catalog generation for Resource Profiles: {error:#}"
                    ));
                    None
                }
            };
            report.warnings.extend(
                resource_profiles
                    .refresh(&config, &profiles, generation, &live_subscription_specs)
                    .warnings,
            );
        }
        if let Some(resync) = resync {
            let passive = match crate::catalog::passive_profiles(&config, &profiles) {
                Ok(passive) => passive,
                Err(error) => {
                    report.errors.push(format!(
                        "derive passive Resource Profile registry: {error:#}"
                    ));
                    agent_spec::profile::ResourceProfileRegistry::empty()
                }
            };
            report.warnings.extend(resync.refresh_with_profiles(
                passive,
                &found.specs,
                &live_subscription_specs,
                this_host,
                &sessions,
                &malformed_declarations,
            ));
        }
    }
    // Auto-archive is an authoring mutation, so it needs the exclusive lock this pass's shared
    // guard blocks. Release the read fence first: the archive step re-discovers under its own
    // exclusive lock, which is what makes its eligibility decision current rather than a snapshot
    // this pass took before it launched anything.
    drop(catalog_lock);
    archive_expired_retirements(root, this_host, &found.specs, &mut report);
    report
}

/// Most seats one auto-archive step may move. A catalog holding hundreds of retirements drains
/// over several passes instead of one pass holding the exclusive authoring lock through all of
/// them — the same bound `MAX_PRESENTATION_PATCHES_PER_PASS` puts on presentation repair.
const MAX_AUTO_ARCHIVED_PER_PASS: usize = 25;

/// Archive retired seats whose grace period expired — the supervisor's half of
/// `st2 catalog archive` (dotfiles#2411, Q11).
///
/// `archive-after "0"` in `catalog.kdl` disables the step entirely. It never queues for the
/// exclusive lock: a pass blocked behind `st2 catalog apply` would stall every live agent's
/// reconciliation, and a due seat is still due next pass. Failures land as warnings, not errors —
/// maintenance must not fail a pass that reconciled correctly.
fn archive_expired_retirements(
    root: &Path,
    this_host: &str,
    specs: &[agent_spec::spec::AgentSpec],
    report: &mut UpReport,
) {
    let grace = match crate::catalog::load(root) {
        Ok(config) => crate::catalog::archive_after(&config),
        Err(error) => {
            report
                .warnings
                .push(format!("read archive-after from catalog.kdl: {error:#}"));
            return;
        }
    };
    if grace.is_zero() || !crate::catalog_archive::pass_has_work(root, this_host, specs, grace) {
        return;
    }

    let attempt =
        crate::catalog_archive::auto_archive(crate::catalog_archive::AutoArchiveRequest {
            catalog: root.to_path_buf(),
            host: this_host.to_owned(),
            grace,
            limit: MAX_AUTO_ARCHIVED_PER_PASS,
        });
    match attempt {
        // Contended: someone is authoring the catalog right now, and the seats stay due.
        Ok(None) => {}
        Ok(Some(result)) => {
            for entry in result.archived {
                tracing::info!(
                    target: "st2",
                    id = %entry.id,
                    to = %entry.to,
                    "archived a retired agent out of the live catalog"
                );
                report.archived.push(entry.id);
            }
            for refusal in result.refused {
                report.warnings.push(format!(
                    "auto-archive skipped {} [{}] {}",
                    refusal.id, refusal.code, refusal.message
                ));
            }
        }
        Err(error) => report
            .warnings
            .push(format!("auto-archive retired agents: {error:#}")),
    }
}

/// A missing Codex, pi, or omp agent must not launch against stale lifecycle hooks. Suppress the
/// affected agent launches (including their sidecars) and surface the error when verification fails.
///
/// Workspace trust belongs to the declared provider command and its selected account-specific
/// runtime. Reconciliation deliberately does not mutate an ambient Codex config: an account selector
/// may choose `CODEX_HOME` only after this process launches the command, so such a write would target
/// the wrong state and could not satisfy the launched seat's trust gate.
fn lifecycle_hook_consumer(needs_codex: bool, needs_pi: bool, needs_omp: bool) -> &'static str {
    match (needs_codex, needs_pi, needs_omp) {
        (true, true, true) => "codex+pi+omp",
        (true, true, false) => "codex+pi",
        (true, false, true) => "codex+omp",
        (false, true, true) => "pi+omp",
        (true, false, false) => "codex",
        (false, true, false) => "pi",
        (false, false, true) => "omp",
        (false, false, false) => unreachable!("gated launch has a lifecycle-hook consumer"),
    }
}

fn gate_harness_launches_on_hooks<'a, V>(
    plan: &mut ReconcilePlan<'a>,
    catalog_root: &Path,
    report: &mut UpReport,
    verify_hooks: V,
) where
    V: FnOnce(Option<&'static str>) -> anyhow::Result<()>,
{
    let mut gated_agents = Vec::new();
    let mut needs_codex = false;
    let mut needs_pi = false;
    let mut needs_omp = false;
    for launch in &plan.launch {
        let mut gated = false;
        for target in &launch.tasks {
            if target.name != "agent" {
                continue;
            }
            let invokes_codex = crate::hooks::launch_invokes_codex(&target.launch, catalog_root);
            let invokes_pi = crate::hooks::launch_invokes_pi(&target.launch, catalog_root);
            let invokes_omp = crate::hooks::launch_invokes_omp(&target.launch, catalog_root);
            needs_codex |= invokes_codex;
            needs_pi |= invokes_pi;
            needs_omp |= invokes_omp;
            gated |= invokes_codex || invokes_pi || invokes_omp;
        }
        if gated {
            gated_agents.push(launch.spec.identity.clone());
        }
    }
    if gated_agents.is_empty() {
        return;
    }

    let consumer = crate::telemetry::tracer_export_enabled()
        .then(|| lifecycle_hook_consumer(needs_codex, needs_pi, needs_omp));
    if let Err(error) = verify_hooks(consumer) {
        plan.launch
            .retain(|launch| !gated_agents.contains(&launch.spec.identity));
        report.errors.push(format!(
            "verify lifecycle hooks for new Codex, pi, or omp agent(s) {}: {error}; launch suppressed",
            gated_agents.join(", ")
        ));
    }
}

/// Root span for one reconcile pass. The compatibility name remains `st2.reconcile_pass`;
/// `span.label` and `st2.reconcile.path` distinguish the bounded path enum.
fn reconcile_span(this_host: &str, path: &'static str) -> tracing::Span {
    tracing::info_span!(
        "st2.reconcile_pass",
        "span.label" = path,
        "st2.host" = this_host,
        "st2.reconcile.path" = path,
        "st2.crash_loops" = tracing::field::Empty,
        "st2.unparked" = tracing::field::Empty,
        "st2.report.errors" = tracing::field::Empty,
        "st2.report.warnings" = tracing::field::Empty,
        "st2.reconcile.skipped" = tracing::field::Empty,
        "st2.result" = tracing::field::Empty,
    )
}

fn catalog_lock_span() -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.catalog.lock",
            "span.label" = "shared",
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn catalog_discover_span() -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.catalog.discover",
            "span.label" = "catalog",
            "st2.catalog.spec_count" = tracing::field::Empty,
            "st2.report.warning_count" = tracing::field::Empty,
            "st2.report.error_count" = tracing::field::Empty,
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn lifecycle_hooks_span(consumer: &'static str) -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.hooks.verify",
            "span.label" = "lifecycle hooks",
            "st2.hooks.consumer" = consumer,
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn catalog_materialize_span(label: &'static str) -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.catalog.materialize",
            "span.label" = label,
            "st2.materialize.failure_count" = tracing::field::Empty,
            "st2.report.warning_count" = tracing::field::Empty,
            "st2.report.error_count" = tracing::field::Empty,
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn runtime_observe_span() -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.runtime.observe",
            "span.label" = "all sessions",
            "st2.runtime.session_count" = tracing::field::Empty,
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn reconcile_execute_span() -> Option<tracing::Span> {
    crate::telemetry::tracer_export_enabled().then(|| {
        tracing::info_span!(
            "st2.reconcile.execute",
            "span.label" = "apply plan",
            "st2.plan.launch_count" = tracing::field::Empty,
            "st2.plan.gc_count" = tracing::field::Empty,
            "st2.plan.teardown_count" = tracing::field::Empty,
            "st2.report.warning_count" = tracing::field::Empty,
            "st2.report.error_count" = tracing::field::Empty,
            "st2.result" = tracing::field::Empty,
        )
    })
}

fn span_count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn finish_child_span(span: Option<&tracing::Span>, failed: bool) {
    let Some(span) = span else {
        return;
    };
    span.record("st2.result", if failed { "fail" } else { "pass" });
    if failed {
        span.set_status(Status::error(""));
    }
}

fn execute_reconcile(
    plan: &ReconcilePlan,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    presentation_cursor: &mut PresentationPatchCursor,
    report: &mut UpReport,
    on_canonical_live: &mut dyn FnMut(&agent_spec::spec::AgentSpec),
) {
    let span = reconcile_execute_span();
    let before = span
        .as_ref()
        .map(|_| (report.warnings.len(), report.errors.len()));
    let _entered = span.as_ref().map(tracing::Span::enter);
    execute_with_presentation_cursor(
        plan,
        runner,
        cap,
        presentation_cursor,
        report,
        on_canonical_live,
    );
    if let (Some(span), Some((warnings_before, errors_before))) = (span.as_ref(), before) {
        span.record("st2.plan.launch_count", span_count(plan.launch.len()));
        span.record("st2.plan.gc_count", span_count(plan.gc.len()));
        span.record("st2.plan.teardown_count", span_count(plan.teardown.len()));
        span.record(
            "st2.report.warning_count",
            span_count(report.warnings.len().saturating_sub(warnings_before)),
        );
        let added_errors = report.errors.len().saturating_sub(errors_before);
        span.record("st2.report.error_count", span_count(added_errors));
        finish_child_span(Some(span), added_errors > 0);
    }
}

/// Stamp bounded pass outcomes onto the root and emit the deterministic per-pass completion log.
/// Call while the span is entered.
fn finish_reconcile_pass(span: &tracing::Span, report: &UpReport) {
    span.record("st2.crash_loops", span_count(report.crash_loops.len()));
    span.record("st2.unparked", span_count(report.unparked.len()));
    span.record("st2.report.errors", span_count(report.errors.len()));
    span.record("st2.report.warnings", span_count(report.warnings.len()));
    span.record("st2.reconcile.skipped", report.skipped);
    let failed = !report.errors.is_empty();
    let result = if failed { "fail" } else { "pass" };
    span.record("st2.result", result);
    if failed {
        span.set_status(Status::error(""));
    }
    tracing::info!(target: "st2", result, "reconcile pass complete");
}

fn finish_failed_reconcile_pass(span: &tracing::Span) {
    span.record("st2.crash_loops", 0_i64);
    span.record("st2.unparked", 0_i64);
    span.record("st2.report.errors", 1_i64);
    span.record("st2.report.warnings", 0_i64);
    span.record("st2.reconcile.skipped", true);
    span.record("st2.result", "fail");
    span.set_status(Status::error(""));
    tracing::info!(target: "st2", result = "fail", "reconcile pass complete");
}

/// One reconcile pass with a throwaway flapping-cap (`st2 up --once`). Returns an owned report;
/// never `Err` — all failures are collected in `report.errors`. The debounce is throwaway too: a
/// single pass has no prior liveness history, so it defers nothing (correct — one-shot has no flicker).
pub fn up_once(root: &Path, this_host: &str, runner: &dyn Runner) -> anyhow::Result<UpReport> {
    let task_context = TaskCompileContext::current(root.to_path_buf())?;
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let started = Instant::now();
    let span = reconcile_span(this_host, "catalog");
    let report = {
        let _entered = span.enter();
        let report = reconcile_pass(root,
        this_host,
        &task_context,
        runner,
        &mut FlappingCap::default(),
        &mut debounce,
        &mut PresentationPatchCursor::default(),
        None, None);
        finish_reconcile_pass(&span, &report);
        report
    };
    crate::metrics::record_reconcile_pass(started.elapsed(), !report.errors.is_empty());
    Ok(report)
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
    let started = Instant::now();
    let span = reconcile_span(this_host, "spec");
    let mut report = UpReport::default();
    {
        let _entered = span.enter();
        if let Err(error) = crate::reconcile::validate_task_identities(specs, this_host) {
            report.errors.push(error.to_string());
        } else {
            let observe_span = runtime_observe_span();
            let observe_entered = observe_span.as_ref().map(tracing::Span::enter);
            let sessions = runner.list_sessions();
            if let (Some(span), Ok(sessions)) = (observe_span.as_ref(), sessions.as_ref()) {
                span.record("st2.runtime.session_count", span_count(sessions.len()));
            }
            finish_child_span(observe_span.as_ref(), sessions.is_err());
            drop(observe_entered);
            match sessions {
                Ok(sessions) => reconcile_specs_with_sessions_in_span(
                    specs,
                    &sessions,
                    this_host,
                    runner,
                    cap,
                    debounce,
                    presentation_cursor,
                    &mut report,
                ),
                Err(error) => {
                    report.skipped = true;
                    report
                        .errors
                        .push(format!("list sessions (pass skipped): {error}"));
                }
            }
        }
        finish_reconcile_pass(&span, &report);
    }
    crate::metrics::record_reconcile_pass(started.elapsed(), !report.errors.is_empty());
    report
}

/// Reconcile an in-memory team against an already captured session snapshot. Eval supervision uses
/// this so crash classification and reconciliation see the same terminal state: otherwise a clean
/// process can exit between two `pty list` calls, be reaped by the second call, then look like a
/// vanished crash on the next tick. The external snapshot deliberately omits
/// `st2.runtime.observe`; its provenance is outside this pass.
pub(crate) fn reconcile_pass_specs_with_sessions(
    specs: &[agent_spec::spec::AgentSpec],
    sessions: &[Session],
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
    presentation_cursor: &mut PresentationPatchCursor,
) -> UpReport {
    let started = Instant::now();
    let span = reconcile_span(this_host, "spec");
    let mut report = UpReport::default();
    {
        let _entered = span.enter();
        reconcile_specs_with_sessions_in_span(
            specs,
            sessions,
            this_host,
            runner,
            cap,
            debounce,
            presentation_cursor,
            &mut report,
        );
        finish_reconcile_pass(&span, &report);
    }
    crate::metrics::record_reconcile_pass(started.elapsed(), !report.errors.is_empty());
    report
}

fn reconcile_specs_with_sessions_in_span(
    specs: &[agent_spec::spec::AgentSpec],
    sessions: &[Session],
    this_host: &str,
    runner: &dyn Runner,
    cap: &mut FlappingCap,
    debounce: &mut LivenessDebounce,
    presentation_cursor: &mut PresentationPatchCursor,
    report: &mut UpReport,
) {
    let now = Instant::now();
    debounce.observe(sessions, now);
    match crate::reconcile(specs, sessions, this_host) {
        Ok(mut plan) => {
            report.deferred = debounce.defer_flickers(&mut plan, now);
            execute_reconcile(
                &plan,
                runner,
                cap,
                presentation_cursor,
                report,
                &mut |_| {},
            );
        }
        Err(error) => report.errors.push(error.to_string()),
    }
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
    let span = reconcile_span(this_host, "selected");
    let result = {
        let _entered = span.enter();
        let result = up_once_selected_specs_with_gates(
            catalog_root,
            specs,
            selector,
            this_host,
            runner,
            |consumer| {
                let hook_span = consumer.and_then(lifecycle_hooks_span);
                let hook_entered = hook_span.as_ref().map(tracing::Span::enter);
                let result = crate::hooks::verify_installed().map(|_| ());
                finish_child_span(hook_span.as_ref(), result.is_err());
                drop(hook_entered);
                result
            },
        );
        match &result {
            Ok(report) => finish_reconcile_pass(&span, report),
            Err(_) => finish_failed_reconcile_pass(&span),
        }
        result
    };
    result
}

/// Discover a folder catalog once, resolve one task before any owner hook/render mutation, then
/// materialize only that owner and execute the selected plan.
pub fn up_once_selected(
    catalog_root: &Path,
    selector: &str,
    this_host: &str,
    runner: &dyn Runner,
) -> anyhow::Result<UpReport> {
    let span = reconcile_span(this_host, "selected");
    let result = {
        let _entered = span.enter();
        let result = (|| {
            let _catalog_lock = {
                let lock_span = catalog_lock_span();
                let lock_entered = lock_span.as_ref().map(tracing::Span::enter);
                let result = crate::CatalogLock::shared(catalog_root)
                    .context("acquire shared catalog-authoring lock for selected reconcile");
                finish_child_span(lock_span.as_ref(), result.is_err());
                drop(lock_entered);
                result?
            };
            let found = {
                let discover_span = catalog_discover_span();
                let discover_entered = discover_span.as_ref().map(tracing::Span::enter);
                let found = crate::discovery::discover(catalog_root);
                if let Some(span) = &discover_span {
                    span.record("st2.catalog.spec_count", span_count(found.specs.len()));
                    span.record("st2.report.warning_count", span_count(found.warnings.len()));
                    span.record("st2.report.error_count", span_count(found.errors.len()));
                    finish_child_span(Some(span), !found.errors.is_empty());
                }
                drop(discover_entered);
                found
            };
            let (owner, _, _) = crate::reconcile::resolve_task(&found.specs, selector, this_host)?;
            let mut report = UpReport::default();
            report.warnings.extend(found.warnings);
            report.errors.extend(
                found
                    .errors
                    .into_iter()
                    .map(|e| format!("{}: {}", e.path.display(), e.message)),
            );
            if let Err(error) = crate::reconcile::validate_task_identities(&found.specs, this_host)
            {
                report.errors.push(error.to_string());
                return Ok(report);
            }
            let owner = owner.clone();
            if crate::hooks::required_by_codex_agent(&owner, this_host, catalog_root) {
                let hook_span = lifecycle_hooks_span("codex");
                let hook_entered = hook_span.as_ref().map(tracing::Span::enter);
                let verification = crate::hooks::verify_installed();
                finish_child_span(hook_span.as_ref(), verification.is_err());
                drop(hook_entered);
                if let Err(error) = verification {
                    report
                        .errors
                        .push(format!("verify lifecycle hooks: {error}"));
                    return Ok(report);
                }
            }
            let materialized = {
                let materialize_span = catalog_materialize_span("selected owner");
                let materialize_entered = materialize_span.as_ref().map(tracing::Span::enter);
                let materialized = crate::materialize::materialize_catalog_against(
                    catalog_root,
                    std::slice::from_ref(&owner),
                    &found.specs,
                    this_host,
                );
                if let Some(span) = &materialize_span {
                    span.record(
                        "st2.materialize.failure_count",
                        span_count(materialized.failed_agents.len()),
                    );
                    span.record(
                        "st2.report.warning_count",
                        span_count(materialized.warnings.len()),
                    );
                    span.record(
                        "st2.report.error_count",
                        span_count(materialized.errors.len()),
                    );
                    finish_child_span(Some(span), !materialized.errors.is_empty());
                }
                drop(materialize_entered);
                materialized
            };
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
                |_| Ok(()),
            )?;
            report.absorb(execution);
            Ok(report)
        })();
        match &result {
            Ok(report) => finish_reconcile_pass(&span, report),
            Err(_) => finish_failed_reconcile_pass(&span),
        }
        result
    };
    result
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
    V: FnOnce(Option<&'static str>) -> anyhow::Result<()>,
{
    crate::reconcile::resolve_task(specs, selector, this_host)?;
    crate::reconcile::validate_task_identities(specs, this_host)?;
    let task_context = TaskCompileContext::current(catalog_root.to_path_buf())?;
    let mut compiled_specs = specs.to_vec();
    compile_generated_tasks(&mut compiled_specs, this_host, &task_context)?;
    let sessions = {
        let observe_span = runtime_observe_span();
        let observe_entered = observe_span.as_ref().map(tracing::Span::enter);
        let sessions = runner.list_sessions();
        if let (Some(span), Ok(sessions)) = (observe_span.as_ref(), sessions.as_ref()) {
            span.record("st2.runtime.session_count", span_count(sessions.len()));
        }
        finish_child_span(observe_span.as_ref(), sessions.is_err());
        drop(observe_entered);
        sessions.map_err(|e| anyhow::anyhow!("list sessions: {e}"))?
    };
    let mut plan =
        crate::reconcile::reconcile_selected(&compiled_specs, &sessions, this_host, selector)?;
    let mut report = UpReport::default();
    gate_harness_launches_on_hooks(&mut plan, catalog_root, &mut report, verify_hooks);
    execute_reconcile(
        &plan,
        runner,
        &mut FlappingCap::default(),
        &mut PresentationPatchCursor::default(),
        &mut report,
        &mut |_| {},
    );
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
    let mut recurring_warnings = RecurringWarnings::default();
    let park_channel = ParkChannel::for_supervisor(root, this_host);
    loop {
        let mut pre = UpReport::default();
        park_channel.grant_requests(&mut cap, &mut pre);
        let mut report = reconcile_pass_specs_with_cursor(
            specs,
            this_host,
            runner,
            &mut cap,
            &mut debounce,
            &mut presentation_cursor,
        );
        pre.absorb(report);
        report = pre;
        for id in &report.unparked {
            reported_flapping.remove(id);
        }
        for cl in &report.crash_loops {
            if reported_flapping.insert(cl.pty_id.clone()) {
                // Counted once per park (the initial transition), not per pass: a task stays
                // parked, so per-pass counting would inflate crash_loops_total unboundedly.
                crate::metrics::record_crash_loop();
                tracing::error!(
                    "st2: GAVE UP on '{id}' — crash-looping past its restart{{}} policy (mode=fail); leaving it parked and its last session for inspection. It is reported as parked by `st2 tasks`. Fix the cause, then `st2 unpark {id}` — no supervisor restart needed.",
                    id = cl.pty_id
                );
                surface_crash_loop(root, this_host, cl);
            }
        }
        recurring_warnings.filter(&mut report);
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
    crate::event::clear_owner_binding(root, this_host);
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

#[derive(Debug, PartialEq, Eq)]
enum ReconcileWake {
    Change,
    Interval,
    Stop,
}

/// Wait for the next reconciliation trigger: a declaration change, the timer fallback, or a stop.
/// Stop stays responsive in bounded slices; a disconnected watcher channel must never masquerade
/// as a change — that turned the nominal timer fallback into a tight full-catalog reconcile loop.
fn wait_for_reconcile(rx: &Receiver<()>, interval: Duration, stop: &AtomicBool) -> ReconcileWake {
    let deadline = Instant::now() + interval;
    loop {
        if stop.load(Ordering::SeqCst) {
            return ReconcileWake::Stop;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return ReconcileWake::Interval;
        };
        let slice = remaining.min(Duration::from_millis(250));
        match rx.recv_timeout(slice) {
            Ok(()) => {
                drain(rx); // coalesce a burst of events into one pass
                return ReconcileWake::Change;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(slice),
        }
    }
}

/// Install independent commit and declaration wake channels. Each channel diagnoses and retries
/// its own setup failure while the timer remains the correctness fallback for both.
fn best_effort_catalog_watcher(
    root: &Path,
    tx: Sender<()>,
) -> Option<crate::watch::CatalogReconcileWatcher> {
    Some(crate::watch::watch_catalog_reconcile_inputs(root, tx))
}

/// Suppresses warnings that persist across passes while still re-surfacing one that clears and
/// returns. An unchanged advisory failure (a non-Git workspace failing its git-exclude, say) must
/// be diagnosed once, not once per reconcile pass.
#[derive(Default)]
struct RecurringWarnings {
    emitted: HashSet<String>,
}

impl RecurringWarnings {
    fn filter(&mut self, report: &mut UpReport) {
        let current: HashSet<_> = report.warnings.iter().cloned().collect();
        self.emitted.retain(|warning| current.contains(warning));
        report
            .warnings
            .retain(|warning| self.emitted.insert(warning.clone()));
    }
}

/// The supervisor loop: reconcile on a timer AND on folder changes until interrupted. The fs-watch is
/// best-effort; the `interval` timer is the always-on fallback. `on_report` is called once per pass
pub fn up_loop(
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    interval: Duration,
    on_report: impl FnMut(&UpReport),
) -> anyhow::Result<()> {
    install_signal_handler();
    up_loop_until(
        root,
        this_host,
        runner,
        interval,
        &STOP,
        best_effort_catalog_watcher,
        on_report,
    )
}

fn up_loop_until(
    root: &Path,
    this_host: &str,
    runner: &dyn Runner,
    interval: Duration,
    stop: &AtomicBool,
    install_watcher: impl FnOnce(&Path, Sender<()>) -> Option<crate::watch::CatalogReconcileWatcher>,
    mut on_report: impl FnMut(&UpReport),
) -> anyhow::Result<()> {
    let task_context = TaskCompileContext::current(root.to_path_buf())?;
    let (tx, rx) = channel::<()>();
    let mut watcher = install_watcher(root, tx);
    let mut cap = FlappingCap::default();
    // Carries per-id liveness across passes so a transient `pty list` flicker under load isn't
    // destructively GC'd (R21c). Fresh throwaway in `up_once` — a single pass has no flicker to absorb.
    let mut debounce = LivenessDebounce::new(DEBOUNCE_GRACE);
    let mut presentation_cursor = PresentationPatchCursor::default();

    // Surface each parked crash-loop once (not every pass): an stderr line AND a message to the
    // agent's supervisor over the native bus, so a crash-loop isn't only visible to whoever is
    // watching the log.
    // Profile parsing and stream-owner publication both belong behind the catalog read fence.
    // Defer them together until the first readable pass: an incomplete catalog apply keeps a
    // resident supervisor alive and retrying without exposing declarations or starting runtime I/O.
    // Once initialized, every reconcile pass reloads profiles and atomically replaces the registry
    // with the watch set; malformed later edits install an empty, fail-closed profile set.
    let mut resync = None;
    let mut resource_profiles = None;
    let mut reported_flapping: HashSet<String> = HashSet::new();
    let mut recurring_warnings = RecurringWarnings::default();
    let park_channel = ParkChannel::for_supervisor(root, this_host);

    loop {
        let mut pre = UpReport::default();
        park_channel.grant_requests(&mut cap, &mut pre);
        if resync.is_none() {
            let catalog_lock = match crate::CatalogLock::shared(root) {
                Ok(lock) => lock,
                Err(error) => {
                    pre.skipped = true;
                    pre.errors.push(format!(
                        "acquire shared catalog-authoring lock for resident initialization (pass skipped): {error:#}"
                    ));
                    on_report(&pre);
                    if stop.load(Ordering::SeqCst)
                        || wait_for_reconcile(&rx, interval, stop) == ReconcileWake::Stop
                    {
                        break;
                    }
                    continue;
                }
            };
            let (config, profiles) = crate::catalog::declared_profile_catalog(root)
                .context("parse resource profiles in catalog.kdl")?;
            let passive_profiles = crate::catalog::passive_profiles(&config, &profiles)
                .context("derive passive Resource Profile registry")?;
            crate::event::publish_owner_binding_under_lock(root, this_host, &catalog_lock)
                .context("publish machine-local stream owner binding")?;
            resync = Some(crate::resync::ResyncSupervisor::with_profiles(
                root.to_path_buf(),
                this_host.to_owned(),
                passive_profiles,
            ));
            resource_profiles = Some(
                crate::resource_profile_supervisor::ResourceProfileSupervisor::new(
                    root.to_path_buf(),
                    this_host.to_owned(),
                )?,
            );
        }
        let mut report = {
            let started = Instant::now();
            let span = reconcile_span(this_host, "catalog");
            let pass = {
                let _entered = span.enter();
                let pass = reconcile_pass(root,
                this_host,
                &task_context,
                runner,
                &mut cap,
                &mut debounce,
                &mut presentation_cursor,
                resync.as_ref(),
                resource_profiles.as_ref());
                finish_reconcile_pass(&span, &pass);
                pass
            };
            crate::metrics::record_reconcile_pass(started.elapsed(), !pass.errors.is_empty());
            pass
        };
        pre.absorb(report);
        report = pre;
        if let Some(watcher) = &mut watcher {
            watcher.refresh();
        }
        // A recovered task that crash-loops again is a new crash-loop, so it must be able to surface
        // again. Leaving the id in the dedup set would make every park after the first one silent.
        for id in report.unparked.iter() {
            reported_flapping.remove(id);
        }
        recurring_warnings.filter(&mut report);
        park_channel.publish(&cap, &mut report);
        for cl in &report.crash_loops {
            if reported_flapping.insert(cl.pty_id.clone()) {
                // Counted once per park (the initial transition), not per pass: a task stays
                // parked, so per-pass counting would inflate crash_loops_total unboundedly.
                crate::metrics::record_crash_loop();
                tracing::error!(
                    "st2: GAVE UP on '{id}' — crash-looping past its restart{{}} policy (mode=fail); leaving it parked and its last session for inspection. It is reported as parked by `st2 tasks`. Fix the cause, then `st2 unpark {id}` — no supervisor restart needed.",
                    id = cl.pty_id
                );
                surface_crash_loop(root, this_host, cl);
            }
        }
        on_report(&report);

        if stop.load(Ordering::SeqCst) {
            break;
        }

        // Wait for a declaration change or the timer fallback in stop-responsive slices.
        if wait_for_reconcile(&rx, interval, stop) == ReconcileWake::Stop {
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
///
/// `supervisor` is a declaration key, never a route: it is resolved through the same two exact
/// readings [`crate::supervisor_chain::resolve_spec`] walks the org chart with, so a parent that
/// declares an `address` keeps receiving its children's crash-loop notices. An address is a
/// routing alias for humans and messages, and this edge is neither.
pub fn surface_crash_loop(catalog_root: &Path, this_host: &str, cl: &CrashLoop) {
    let agent = cl.agent_bus_id(this_host);
    let Some(supervisor) = cl.supervisor.as_deref() else {
        tracing::warn!(
            "st2: crash-loop '{}' ({agent}) has no supervisor to notify.",
            cl.pty_id
        );
        return;
    };
    let Ok(Some(agent_dir)) = message::resolve_declared_dir(catalog_root, supervisor, this_host)
    else {
        tracing::warn!(
            "st2: crash-loop '{}': supervisor '{supervisor}' not found in the catalog to notify.",
            cl.pty_id
        );
        return;
    };
    let subject = format!("crash-loop: {agent} parked");
    // `st2 unpark` relaunches into the identical failure when the cause is structural, so the
    // notice must not offer it as a recovery verb: acting on that advice restarts the storm. The
    // test is the same predicate admission uses, not the wording of a spawn error.
    let unbindable_socket =
        session_socket_overage(&effective_pty_root(catalog_root), &cl.pty_id);
    let body = match &unbindable_socket {
        Some((socket, over)) => format!(
            "st2 gave up restarting task '{id}' (agent {agent}) — it crash-looped past its \
             restart{{}} policy (mode=fail) and is parked. The cause is structural and `st2 \
             unpark` cannot recover it: the task's session socket path {socket} is {bytes} bytes, \
             exceeding the {limit}-byte portable limit by {over}, so every launch fails the same \
             way. Shorten the identity or task id by at least {over} bytes, or declare a shorter \
             pty root; the declaration has to change before this task can run.",
            id = cl.pty_id,
            socket = socket.display(),
            bytes = PORTABLE_SOCKET_PATH_LIMIT + over,
            limit = PORTABLE_SOCKET_PATH_LIMIT,
        ),
        None => format!(
            "st2 gave up restarting task '{id}' (agent {agent}) — it crash-looped past its \
             restart{{}} policy (mode=fail) and is parked. Its last dead session is left as \
             evidence, and `st2 tasks` reports the park. Investigate the cause, then `st2 unpark \
             {id}` to recover just this task — restarting st2 is not required and would cold-boot \
             every task on the host.",
            id = cl.pty_id
        ),
    };
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
        tracing::warn!(
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
mod tests;
