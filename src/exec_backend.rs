//! The `exec` backend (M1b) — terminal-free process supervision (R09).
//!
//! `pty` tasks go to the `pty` CLI (which allocates a pseudo-terminal — an agent harness). `exec`
//! tasks (the ding, daemons, a stage's script) must NOT allocate a terminal, so st2 supervises them
//! directly: shell source through `sh -c`, or a structured argv with no shell. The process is spawned
//! in its own session (`setsid` — no controlling tty, and decoupled from st2 so it survives st2 exit,
//! Nomad-style), stdout/stderr go to a log file, and the pid is recorded in a machine-local
//! runner-state dir. Liveness is `kill(pid, 0)`; st2 best-effort reaps its own exited children so a
//! zombie never reads as alive. The same `setsid` session doubles as the teardown unit: an explicit
//! kill targets the whole process GROUP, so a task's children die with it (see
//! [`ExecBackend::kill`]) — a plain stop of st2 never touches it.
//!
//! State is machine-local (pids don't sync across hosts) and keyed by host, so a restarted st2 on the
//! same host adopts the exec processes it left running.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::host_lock::process_alive;
use crate::reconcile::{Session, TaskLaunch, TaskTarget};
use crate::run::resolve_task_cwd;

const EXEC_GENERATION_SCHEMA: &str = "st2.exec-generation.v1";
const RETIRE_TERM_WAIT: Duration = Duration::from_secs(2);
const RETIRE_KILL_WAIT: Duration = Duration::from_secs(2);
const RETIRE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The immutable identity of one exec process generation.
///
/// `start_time_ticks` is the kernel process-start token: Linux clock ticks
/// since boot, or the macOS start timestamp in microseconds. Pairing it with
/// the pid prevents a reused pid from identifying a different generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecGeneration {
    pub schema: String,
    pub pid: u32,
    pub created_at: String,
    pub start_time_ticks: u64,
    pub generation_id: String,
}

/// Read-only identity observation for strict and retained plain-PID records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecGenerationObservation {
    Running {
        pid: u32,
        created_at: String,
        generation_id: String,
        start_time_ticks: u64,
    },
    Exited {
        pid: u32,
        created_at: String,
        generation_id: String,
    },
    Indeterminate {
        reason: String,
        /// Conservative projection for the reconciler's existing boolean
        /// session boundary. Unknown live ownership must not launch a duplicate.
        alive_for_reconcile: bool,
    },
}

/// Supervises `exec` tasks as terminal-free processes.
pub struct ExecBackend {
    /// Where `<id>.pid` lives (machine-local; not synced).
    state_dir: PathBuf,
    /// The catalog root — the value of `$CATALOG` during expansion.
    catalog_root: PathBuf,
    /// Direct children published by this backend instance. The kernel remains
    /// the ownership authority: this set only decides which `waitpid` calls are
    /// permitted, never whether a foreign generation may be signalled.
    owned_children: Mutex<HashSet<u32>>,
}

impl ExecBackend {
    pub fn new(state_dir: PathBuf, catalog_root: PathBuf) -> Self {
        Self {
            state_dir,
            catalog_root,
            owned_children: Mutex::new(HashSet::new()),
        }
    }

    fn pid_path(&self, id: &str) -> PathBuf {
        self.state_dir.join(format!("{id}.pid"))
    }
    /// Where an exec's stdout+stderr are auto-logged: `<catalog>/logs/<full-label>.log` — a
    /// discoverable, greppable location keyed by the task's full label (e.g. `logs/mix.sup.ding.log`).
    /// Execs run as detached sidecars, so without this a wedged/crashed ding's output vanishes; here it
    /// stays inspectable after the fact. (Distinct from the machine-local pid state in `state_dir`.)
    fn log_path(&self, id: &str) -> PathBuf {
        self.catalog_root.join("logs").join(format!("{id}.log"))
    }

    /// The one bounded prior generation retained across a crash restart.
    fn previous_log_path(&self, id: &str) -> PathBuf {
        self.catalog_root.join("logs").join(format!("{id}.log.1"))
    }

    /// Spawn `target` as a terminal-free, detached process and record its pid.
    pub fn spawn(&self, target: &TaskTarget, spec_dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        let cwd = resolve_task_cwd(target, spec_dir, &self.catalog_root);
        let log_file = self.log_path(&target.pty_id);
        if let Some(parent) = log_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let log = fs::File::options()
            .create(true)
            .append(true)
            .open(&log_file)?;

        // Spawn the task into its own isolation domain (R21b): on systemd Linux, its own transient
        // scope (own cgroup, sibling of the transport unit) so a transport/supervisor cgroup-cascade
        // kill cannot take it; elsewhere a plain pass-through detached by the `setsid` below. Env, cwd,
        // and stdio set here reach the task in both modes.
        let unit = crate::isolate::scope_unit(&target.pty_id);
        let (program, args): (OsString, Vec<OsString>) = match &target.launch {
            TaskLaunch::Shell(command) => (
                OsString::from("sh"),
                vec![OsString::from("-c"), OsString::from(command)],
            ),
            TaskLaunch::Argv(argv) => {
                debug_assert!(!argv.is_empty());
                let mut expanded = argv
                    .iter()
                    .map(|arg| {
                        OsString::from(crate::expand::expand_catalog(arg, &self.catalog_root))
                    })
                    .collect::<Vec<_>>();
                let program = expanded.remove(0);
                (program, expanded)
            }
        };
        let arg_refs = args.iter().map(OsString::as_os_str).collect::<Vec<_>>();
        let mut cmd = crate::isolate::wrap(&unit, &program, &arg_refs);
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .env("CATALOG", &self.catalog_root)
            .env("ST_ROOT", &self.catalog_root)
            .env(
                "PTY_ROOT",
                crate::run::effective_pty_root(&self.catalog_root),
            );
        if let Ok(path) = crate::hooks::hooks_root() {
            cmd.env("ST_HOOKS", path);
        }
        for (k, v) in &target.env {
            // PTY_ROOT resolves to the EFFECTIVE root (an exported ambient one wins over the rendered
            // `$CATALOG/pty`) so a ding — an exec task that reads PTY_ROOT to find the pty it pokes —
            // targets the same partition st2's pty ops use.
            if k == "PTY_ROOT" {
                cmd.env(
                    "PTY_ROOT",
                    crate::run::effective_pty_root(&self.catalog_root),
                );
            } else {
                cmd.env(k, crate::expand::expand_catalog(v, &self.catalog_root));
            }
        }
        // New session: no controlling terminal (R09) and decoupled from st2's process group, so it
        // survives st2 exit and `kill(-pid)` teardown reaps the whole group. In Scope mode this runs
        // on the outer `systemd-run`, which exec-chains into the workload, so the workload is the
        // session leader and `child.id()` is its pid. Still st2's child, so st2 can reap it (see `list`).
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        self.record_spawned_generation(&target.pty_id, child.id())?;
        // Detach: dropping Child neither waits nor kills; `list` reaps exited children.
        drop(child);
        Ok(())
    }

    /// List known exec tasks with liveness. Best-effort reaps our own exited children first so a
    /// zombie (whose pid still exists) is not mistaken for alive.
    pub fn list(&self) -> anyhow::Result<Vec<Session>> {
        self.reap_owned_children();
        let mut out = Vec::new();
        let dir = match fs::read_dir(&self.state_dir) {
            Ok(d) => d,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(error.into()),
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pid") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let observation = self.observe_generation_path(id, &path);
            let alive = observation_alive_for_reconcile(observation);
            out.push(Session {
                pty_id: id.to_string(),
                alive,
                exit_code: None,
                presentation: None,
            });
        }
        Ok(out)
    }

    /// SIGTERM the whole task — the process GROUP, not just the recorded pid. `spawn` puts each exec
    /// task in its own session via `setsid`, so the recorded pid begins as the group leader
    /// (pgid == pid), and `kill(-pid)` reaches the leader and anything it forked. Killing only the
    /// leader could leave the real workload orphaned.
    ///
    /// This is legacy lifecycle behavior, not race-free retirement: generation observation and
    /// numeric process-group signaling are separate operations, so a PID/PGID can be reused between
    /// them. Issue #121 owns capability-pinned signaling and exact record retirement.
    pub fn kill(&self, id: &str) -> anyhow::Result<()> {
        let pid = match self.observe_generation(id)? {
            ExecGenerationObservation::Running { pid, .. } => pid as i32,
            ExecGenerationObservation::Exited { .. } => return Ok(()),
            ExecGenerationObservation::Indeterminate { reason, .. } => {
                anyhow::bail!("exec generation for '{id}' is indeterminate: {reason}")
            }
        };
        // Negative target = the process group led by `pid`.
        let ret = unsafe { libc::kill(-pid, libc::SIGTERM) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // ESRCH (the whole group is already gone) is not a failure for our purposes.
            if err.raw_os_error() != Some(libc::ESRCH) {
                anyhow::bail!("kill exec group '{id}' (pgid {pid}): {err}");
            }
        }
        Ok(())
    }

    /// Retire one exact recorded generation, including every process in its session, before
    /// deleting its runner-state. A cooperative task gets a bounded SIGTERM grace period; an
    /// ignoring task is escalated to SIGKILL. The generation record remains present throughout so
    /// a failed/aborted retirement cannot turn a live adapter into an untracked orphan.
    pub fn retire(&self, id: &str) -> anyhow::Result<()> {
        let expected = match self.observe_generation(id)? {
            ExecGenerationObservation::Running {
                pid,
                created_at,
                generation_id,
                ..
            } => (pid, created_at, generation_id),
            ExecGenerationObservation::Exited { .. } => {
                self.remove(id)?;
                return Ok(());
            }
            ExecGenerationObservation::Indeterminate { reason, .. } => {
                anyhow::bail!("exec generation for '{id}' is indeterminate: {reason}")
            }
        };

        self.signal_expected_generation(id, &expected, libc::SIGTERM)?;
        if self.wait_for_group_exit(expected.0 as i32, RETIRE_TERM_WAIT) {
            self.remove(id)?;
            return Ok(());
        }

        self.signal_expected_generation(id, &expected, libc::SIGKILL)?;
        if !self.wait_for_group_exit(expected.0 as i32, RETIRE_KILL_WAIT) {
            anyhow::bail!(
                "exec process group {} for '{id}' survived bounded SIGKILL retirement",
                expected.0
            );
        }
        self.remove(id)?;
        Ok(())
    }

    fn signal_expected_generation(
        &self,
        id: &str,
        expected: &(u32, String, String),
        signal: i32,
    ) -> anyhow::Result<()> {
        match self.observe_generation(id)? {
            ExecGenerationObservation::Running {
                pid,
                created_at,
                generation_id,
                ..
            } if pid == expected.0 && created_at == expected.1 && generation_id == expected.2 => {}
            ExecGenerationObservation::Exited {
                pid,
                created_at,
                generation_id,
            } if pid == expected.0 && created_at == expected.1 && generation_id == expected.2 => {
                return Ok(());
            }
            other => {
                anyhow::bail!("exec generation for '{id}' changed during retirement: {other:?}")
            }
        }
        let result = unsafe { libc::kill(-(expected.0 as i32), signal) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                anyhow::bail!(
                    "signal {signal} to exec group '{}' (pgid {}): {error}",
                    id,
                    expected.0
                );
            }
        }
        Ok(())
    }

    fn wait_for_group_exit(&self, pgid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.reap_owned_children();
            if !process_group_has_live_member(pgid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(RETIRE_POLL_INTERVAL);
        }
    }

    /// Reap a crashed exec before restarting it: remove only the stale pid and rotate the current
    /// diagnostic log to one bounded prior generation. The next spawn creates a fresh current log,
    /// leaving `<id>.log` + `<id>.log.1` inspectable without unbounded crash-loop accumulation.
    pub fn reap_for_restart(&self, id: &str) -> anyhow::Result<()> {
        let current = self.log_path(id);
        if current.exists() {
            let previous = self.previous_log_path(id);
            let _ = fs::remove_file(&previous);
            fs::rename(&current, &previous)?;
        }
        // Remove the pid only after diagnostics are safely rotated. If rotation fails, retaining the
        // dead pid lets the next reconciliation retry instead of treating the task as a fresh launch
        // and appending a new execution to the unrotated log.
        let _ = fs::remove_file(self.pid_path(id));
        Ok(())
    }

    /// Finally remove an exec task's runner-state and both bounded log generations. This is the
    /// retirement/final-GC path, not ordinary crash relaunch.
    pub fn remove(&self, id: &str) -> anyhow::Result<()> {
        let _ = fs::remove_file(self.pid_path(id));
        let _ = fs::remove_file(self.log_path(id));
        let _ = fs::remove_file(self.previous_log_path(id));
        Ok(())
    }

    /// Observe one exec without ever treating a reused pid as its recorded
    /// generation.
    pub fn observe_generation(&self, id: &str) -> anyhow::Result<ExecGenerationObservation> {
        self.observe_generation_optional(id)?
            .ok_or_else(|| anyhow::anyhow!("reading pid for exec '{id}': no generation record"))
    }

    /// Observe exactly one desired exec id without changing its state record or
    /// lifecycle. `None` means only that the record is absent.
    pub fn observe_generation_optional(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<ExecGenerationObservation>> {
        self.reap_owned_children();
        let path = self.pid_path(id);
        match open_generation_record(&path) {
            Ok(record) => Ok(Some(observe_open_generation(id, &path, record, || {}))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Ok(Some(indeterminate(
                format!("opening exec generation {}: {error}", path.display()),
                true,
            ))),
        }
    }

    fn observe_generation_path(&self, id: &str, path: &Path) -> ExecGenerationObservation {
        match open_generation_record(path) {
            Ok(record) => observe_open_generation(id, path, record, || {}),
            Err(error) => indeterminate(
                format!("opening exec generation {}: {error}", path.display()),
                error.kind() != std::io::ErrorKind::NotFound,
            ),
        }
    }

    fn record_spawned_generation(&self, id: &str, pid: u32) -> anyhow::Result<()> {
        let result = (|| {
            let start_time_ticks = process_start_time_ticks(pid as i32)
                .with_context(|| format!("identifying spawned exec generation '{id}'"))?;
            let created_at = rfc3339_utc(SystemTime::now())
                .with_context(|| format!("timestamping exec generation '{id}'"))?;
            let generation = ExecGeneration {
                schema: EXEC_GENERATION_SCHEMA.to_string(),
                pid,
                generation_id: generation_id(id, pid, &created_at, start_time_ticks),
                created_at,
                start_time_ticks,
            };
            self.publish_generation(id, &generation)
        })();
        if let Err(error) = result {
            // Publication is the ownership boundary. Never leave a live process
            // behind when its exact generation could not be recorded.
            terminate_unpublished(pid);
            return Err(error);
        }
        self.owned_children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pid);
        Ok(())
    }

    /// Reap only children spawned and successfully published by this backend
    /// instance. In particular, never call `waitpid` merely because a state
    /// record names a pid: after a restart or record mismatch that pid may
    /// belong to a foreign process.
    ///
    /// Darwin can report an owned zombie as present to `kill(pid, 0)` while
    /// `proc_pidinfo` can no longer return its start token. Reaping before the
    /// strict observation turns that exact child into positive absence without
    /// weakening the generation proof for non-children.
    fn reap_owned_children(&self) {
        let mut owned = self
            .owned_children
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        owned.retain(|pid| {
            loop {
                let result =
                    unsafe { libc::waitpid(*pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
                if result == *pid as i32 {
                    break false;
                }
                if result == 0 {
                    break true;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // ECHILD means this process no longer owns the pid. Discard the
                // capability; subsequent observations remain identity-checked and
                // conservative.
                break error.raw_os_error() != Some(libc::ECHILD);
            }
        });
    }

    fn publish_generation(&self, id: &str, generation: &ExecGeneration) -> anyhow::Result<()> {
        let path = self.pid_path(id);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("exec generation path has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut bytes = serde_json::to_vec(generation)?;
        bytes.push(b'\n');
        let mut temp = tempfile::Builder::new()
            .prefix(".exec-generation.")
            .tempfile_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("publishing exec generation {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn process_group_has_live_member(pgid: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after_comm) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
            continue;
        };
        let mut fields = after_comm.split_whitespace();
        let state = fields.next();
        let _parent = fields.next();
        let process_group = fields.next().and_then(|field| field.parse::<i32>().ok());
        if process_group == Some(pgid) && state != Some("Z") {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn process_group_has_live_member(pgid: i32) -> bool {
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

const MAX_LEGACY_PID_RECORD_BYTES: u64 = 64;
const MAX_EXEC_GENERATION_RECORD_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordEvidence {
    device: u64,
    inode: u64,
    links: u64,
    length: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl RecordEvidence {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            length: metadata.len(),
            mode: metadata.mode(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

fn open_generation_record(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(test)]
fn open_legacy_pid_record(path: &Path) -> std::io::Result<File> {
    open_generation_record(path)
}

fn read_generation_record(
    record: &mut File,
    max_bytes: u64,
    kind: &str,
) -> std::io::Result<Vec<u8>> {
    record.seek(SeekFrom::Start(0))?;
    let mut raw = Vec::new();
    record.take(max_bytes + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{kind} record exceeds {max_bytes} bytes"),
        ));
    }
    Ok(raw)
}

fn read_legacy_pid_record(record: &mut File) -> std::io::Result<Vec<u8>> {
    read_generation_record(record, MAX_LEGACY_PID_RECORD_BYTES, "legacy pid")
}

fn observe_open_generation(
    id: &str,
    path: &Path,
    mut record: File,
    midpoint: impl FnOnce(),
) -> ExecGenerationObservation {
    let preview = match read_generation_record(
        &mut record,
        MAX_EXEC_GENERATION_RECORD_BYTES,
        "exec generation",
    ) {
        Ok(raw) => raw,
        Err(error) => {
            return indeterminate(format!("reading {}: {error}", path.display()), true);
        }
    };
    let is_strict =
        std::str::from_utf8(&preview).is_ok_and(|raw| raw.trim_start().starts_with('{'));
    if is_strict {
        observe_open_strict_generation(id, path, record, midpoint)
    } else {
        observe_open_legacy_generation(id, path, record, midpoint)
    }
}

fn observe_open_strict_generation(
    id: &str,
    path: &Path,
    mut record: File,
    midpoint: impl FnOnce(),
) -> ExecGenerationObservation {
    let initial_metadata = match record.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return indeterminate("strict generation record is not a regular file", true),
        Err(error) => {
            return indeterminate(
                format!("cannot stat strict generation record: {error}"),
                true,
            );
        }
    };
    let initial_evidence = RecordEvidence::from_metadata(&initial_metadata);
    let raw = match read_generation_record(
        &mut record,
        MAX_EXEC_GENERATION_RECORD_BYTES,
        "strict generation",
    ) {
        Ok(raw) => raw,
        Err(error) => {
            return indeterminate(format!("reading {}: {error}", path.display()), true);
        }
    };
    let generation: ExecGeneration = match serde_json::from_slice(&raw) {
        Ok(generation) => generation,
        Err(error) => {
            return indeterminate(format!("strict JSON parse failed: {error}"), true);
        }
    };
    midpoint();
    if let Err(reason) = validate_generation(id, &generation) {
        return indeterminate(reason, true);
    }
    if let Err(reason) = prove_generation_record_unchanged(
        path,
        &mut record,
        initial_evidence,
        &raw,
        MAX_EXEC_GENERATION_RECORD_BYTES,
        "strict generation",
    ) {
        return indeterminate(reason, true);
    }
    match generation_process_state(&generation) {
        GenerationProcessState::Running => ExecGenerationObservation::Running {
            pid: generation.pid,
            created_at: generation.created_at,
            generation_id: generation.generation_id,
            start_time_ticks: generation.start_time_ticks,
        },
        GenerationProcessState::Exited => ExecGenerationObservation::Exited {
            pid: generation.pid,
            created_at: generation.created_at,
            generation_id: generation.generation_id,
        },
        GenerationProcessState::Mismatch => {
            indeterminate("recorded startTimeTicks does not match the live pid", true)
        }
    }
}

fn observe_open_legacy_generation(
    id: &str,
    path: &Path,
    mut record: File,
    midpoint: impl FnOnce(),
) -> ExecGenerationObservation {
    let initial_metadata = match record.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return indeterminate("legacy pid record is not a regular file", true),
        Err(error) => {
            return indeterminate(format!("cannot stat legacy pid record: {error}"), true);
        }
    };
    let initial_evidence = RecordEvidence::from_metadata(&initial_metadata);
    let raw = match read_legacy_pid_record(&mut record) {
        Ok(raw) => raw,
        Err(error) => {
            return indeterminate(format!("reading {}: {error}", path.display()), true);
        }
    };
    let raw_text = match std::str::from_utf8(&raw) {
        Ok(raw) => raw,
        Err(error) => {
            return indeterminate(
                format!(
                    "parsing legacy pid record {} as UTF-8: {error}",
                    path.display()
                ),
                true,
            );
        }
    };
    let pid = match raw_text.trim().parse::<i32>() {
        Ok(pid) => pid,
        Err(error) => {
            return indeterminate(
                format!("parsing legacy pid record {}: {error}", path.display()),
                true,
            );
        }
    };
    midpoint();
    if pid <= 0 || !process_alive(pid) {
        return indeterminate("legacy pid is not a live process", process_alive(pid));
    }
    let start_time_ticks = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(error) => {
            return indeterminate(
                format!("cannot read legacy process start token: {error:#}"),
                true,
            );
        }
    };
    let modified = match initial_metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            return indeterminate(
                format!("legacy pid file has no usable mtime: {error}"),
                true,
            );
        }
    };
    match legacy_pid_predates_record(start_time_ticks, modified) {
        Ok(true) => {}
        Ok(false) => {
            return indeterminate(
                "legacy pid file predates the current process generation",
                true,
            );
        }
        Err(error) => {
            return indeterminate(
                format!("cannot order legacy pid file and process start: {error:#}"),
                true,
            );
        }
    }
    if let Err(reason) = prove_generation_record_unchanged(
        path,
        &mut record,
        initial_evidence,
        &raw,
        MAX_LEGACY_PID_RECORD_BYTES,
        "legacy pid",
    ) {
        return indeterminate(reason, true);
    }
    // Close the observation window on both resources: neither a changed path
    // record nor a changed process generation is ever promoted.
    if process_start_time_ticks(pid).ok() != Some(start_time_ticks) {
        return indeterminate("legacy process generation changed while observed", true);
    }
    let created_at = match process_created_at(start_time_ticks).and_then(rfc3339_utc) {
        Ok(value) => value,
        Err(error) => {
            return indeterminate(
                format!("cannot encode legacy process start time: {error:#}"),
                true,
            );
        }
    };
    let generation_id = crate::task_inventory::generation_id(
        "exec",
        id,
        pid as u32,
        &created_at,
        Some(start_time_ticks),
    );
    ExecGenerationObservation::Running {
        pid: pid as u32,
        created_at,
        generation_id,
        start_time_ticks,
    }
}

fn prove_generation_record_unchanged(
    path: &Path,
    retained: &mut File,
    initial_evidence: RecordEvidence,
    initial_raw: &[u8],
    max_bytes: u64,
    kind: &str,
) -> Result<(), String> {
    let retained_raw = read_generation_record(retained, max_bytes, kind)
        .map_err(|error| format!("re-reading retained {kind} record: {error}"))?;
    let retained_metadata = retained
        .metadata()
        .map_err(|error| format!("re-statting retained {kind} record: {error}"))?;
    if RecordEvidence::from_metadata(&retained_metadata) != initial_evidence
        || retained_raw != initial_raw
    {
        return Err(format!("{kind} record changed while observed"));
    }

    // Re-open by name without following symlinks. The retained descriptor keeps
    // the original inode allocated, so an atomic replacement cannot reuse its
    // identity while this comparison is in progress.
    let mut current = open_generation_record(path)
        .map_err(|error| format!("re-opening {kind} record by name: {error}"))?;
    let current_metadata_before = current
        .metadata()
        .map_err(|error| format!("stating current {kind} record: {error}"))?;
    if !current_metadata_before.is_file() {
        return Err(format!("current {kind} record is not a regular file"));
    }
    let current_raw = read_generation_record(&mut current, max_bytes, kind)
        .map_err(|error| format!("re-reading current {kind} record: {error}"))?;
    let current_metadata_after = current
        .metadata()
        .map_err(|error| format!("re-statting current {kind} record: {error}"))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("checking final {kind} record path: {error}"))?;
    if RecordEvidence::from_metadata(&current_metadata_before) != initial_evidence
        || RecordEvidence::from_metadata(&current_metadata_after) != initial_evidence
        || RecordEvidence::from_metadata(&path_metadata) != initial_evidence
        || current_raw != initial_raw
    {
        return Err(format!("{kind} record path changed while observed"));
    }
    Ok(())
}

fn terminate_unpublished(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        loop {
            let result = libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}

fn indeterminate(
    reason: impl Into<String>,
    alive_for_reconcile: bool,
) -> ExecGenerationObservation {
    ExecGenerationObservation::Indeterminate {
        reason: reason.into(),
        alive_for_reconcile,
    }
}

fn observation_alive_for_reconcile(observation: ExecGenerationObservation) -> bool {
    match observation {
        ExecGenerationObservation::Running { .. } => true,
        ExecGenerationObservation::Exited { .. } => false,
        ExecGenerationObservation::Indeterminate {
            alive_for_reconcile,
            ..
        } => alive_for_reconcile,
    }
}

fn validate_generation(id: &str, generation: &ExecGeneration) -> Result<(), String> {
    if generation.schema != EXEC_GENERATION_SCHEMA {
        return Err(format!("unsupported schema {:?}", generation.schema));
    }
    if generation.pid == 0 || generation.pid > i32::MAX as u32 {
        return Err("pid must be positive".into());
    }
    if generation.start_time_ticks == 0 {
        return Err("startTimeTicks must be positive".into());
    }
    if !crate::task_inventory::is_rfc3339_utc_millis(&generation.created_at) {
        return Err("createdAt must be an RFC3339 UTC timestamp with milliseconds".into());
    }
    let expected = generation_id(
        id,
        generation.pid,
        &generation.created_at,
        generation.start_time_ticks,
    );
    if generation.generation_id != expected {
        return Err("generationId does not match the generation fields".into());
    }
    Ok(())
}

enum GenerationProcessState {
    Running,
    Exited,
    Mismatch,
}

fn generation_process_state(generation: &ExecGeneration) -> GenerationProcessState {
    let pid = generation.pid as i32;
    if !process_alive(pid) {
        return GenerationProcessState::Exited;
    }
    if process_start_time_ticks(pid).ok() != Some(generation.start_time_ticks) {
        return if process_alive(pid) {
            GenerationProcessState::Mismatch
        } else {
            GenerationProcessState::Exited
        };
    }
    if !process_alive(pid) {
        GenerationProcessState::Exited
    } else if process_start_time_ticks(pid).ok() == Some(generation.start_time_ticks) {
        GenerationProcessState::Running
    } else {
        GenerationProcessState::Mismatch
    }
}

fn generation_id(runtime_id: &str, pid: u32, created_at: &str, start_time_ticks: u64) -> String {
    crate::task_inventory::generation_id(
        "exec",
        runtime_id,
        pid,
        created_at,
        Some(start_time_ticks),
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn process_start_time_ticks(pid: i32) -> anyhow::Result<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_comm = stat
        .rsplit_once(") ")
        .ok_or_else(|| anyhow::anyhow!("malformed /proc/{pid}/stat"))?
        .1;
    // Fields after comm begin at field 3 (state); starttime is field 22, therefore index 19.
    after_comm
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow::anyhow!("missing starttime in /proc/{pid}/stat"))?
        .parse()
        .with_context(|| format!("parsing starttime in /proc/{pid}/stat"))
}

#[cfg(target_os = "macos")]
pub(crate) fn process_start_time_ticks(pid: i32) -> anyhow::Result<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskallinfo>::zeroed();
    let expected = std::mem::size_of::<libc::proc_taskallinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKALLINFO,
            0,
            info.as_mut_ptr().cast(),
            expected,
        )
    };
    if read != expected {
        anyhow::bail!("proc_pidinfo({pid}) returned {read}, expected {expected}");
    }
    let info = unsafe { info.assume_init() };
    Ok(info
        .pbsd
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbsd.pbi_start_tvusec))
}

#[cfg(target_os = "linux")]
fn legacy_pid_predates_record(start_time_ticks: u64, modified: SystemTime) -> anyhow::Result<bool> {
    let ticks_per_second = clock_ticks_per_second()?;
    let uptime: f64 = fs::read_to_string("/proc/uptime")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing /proc/uptime value"))?
        .parse()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    let modified = modified.duration_since(UNIX_EPOCH)?.as_secs_f64();
    let tick = 1.0 / ticks_per_second as f64;
    // The start token denotes a clock-tick bucket. The record must follow the
    // end of that bucket, otherwise PID reuse cannot be ruled out.
    let latest_start = now - uptime + start_time_ticks as f64 * tick + tick;
    Ok(modified >= latest_start)
}

#[cfg(target_os = "macos")]
fn legacy_pid_predates_record(start_time_ticks: u64, modified: SystemTime) -> anyhow::Result<bool> {
    let modified_micros = modified.duration_since(UNIX_EPOCH)?.as_micros();
    Ok(u128::from(start_time_ticks) <= modified_micros)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> anyhow::Result<u64> {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks <= 0 {
        anyhow::bail!("sysconf(_SC_CLK_TCK) failed");
    }
    Ok(ticks as u64)
}

#[cfg(target_os = "linux")]
fn process_created_at(start_time_ticks: u64) -> anyhow::Result<SystemTime> {
    let boot_seconds = fs::read_to_string("/proc/stat")?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .ok_or_else(|| anyhow::anyhow!("missing btime in /proc/stat"))?
        .parse::<u64>()?;
    let ticks_per_second = clock_ticks_per_second()?;
    let seconds = start_time_ticks / ticks_per_second;
    let remainder = start_time_ticks % ticks_per_second;
    let nanos = remainder.saturating_mul(1_000_000_000) / ticks_per_second;
    Ok(UNIX_EPOCH
        + Duration::from_secs(boot_seconds.saturating_add(seconds))
        + Duration::from_nanos(nanos))
}

#[cfg(target_os = "macos")]
fn process_created_at(start_time_ticks: u64) -> anyhow::Result<SystemTime> {
    Ok(UNIX_EPOCH + Duration::from_micros(start_time_ticks))
}

pub(crate) fn rfc3339_utc(time: SystemTime) -> anyhow::Result<String> {
    let duration = time.duration_since(UNIX_EPOCH)?;
    let seconds = duration.as_secs() as libc::time_t;
    let millis = duration.subsec_millis();
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    if unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) }.is_null() {
        anyhow::bail!("gmtime_r failed");
    }
    let tm = unsafe { tm.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        millis
    ))
}

#[cfg(test)]
mod generation_observation_tests {
    use std::collections::BTreeMap;
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    fn target(id: &str) -> TaskTarget {
        TaskTarget {
            kind: crate::spec::TaskKind::Exec,
            pty_id: id.to_string(),
            bus_id: "host.test".to_string(),
            name: "probe".to_string(),
            derived: false,
            launch: TaskLaunch::Shell("sleep 30".to_string()),
            cwd: None,
            workspace: None,
            tags: BTreeMap::new(),
            env: BTreeMap::new(),
            keep: false,
            presentation: None,
        }
    }

    #[test]
    fn spawn_atomically_publishes_a_strict_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        fs::create_dir_all(&catalog).unwrap();
        let backend = ExecBackend::new(tmp.path().join("state"), catalog);
        let id = "host.test.strict";

        backend.spawn(&target(id), tmp.path()).unwrap();
        let raw = fs::read_to_string(backend.pid_path(id)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "createdAt",
                "generationId",
                "pid",
                "schema",
                "startTimeTicks"
            ]
        );
        let generation: ExecGeneration = serde_json::from_value(value).unwrap();
        assert_eq!(generation.schema, EXEC_GENERATION_SCHEMA);
        assert_eq!(
            generation.generation_id,
            generation_id(
                id,
                generation.pid,
                &generation.created_at,
                generation.start_time_ticks
            )
        );
        assert!(matches!(
            backend.observe_generation(id).unwrap(),
            ExecGenerationObservation::Running { pid, .. } if pid == generation.pid
        ));

        backend.kill(id).unwrap();
        backend.remove(id).unwrap();
    }

    #[test]
    fn owned_exited_child_is_reaped_before_strict_observation() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        fs::create_dir_all(&catalog).unwrap();
        let backend = ExecBackend::new(tmp.path().join("state"), catalog);
        let id = "host.test.owned-zombie";

        let mut immediate = target(id);
        immediate.launch = TaskLaunch::Shell("exit 0".to_string());
        backend.spawn(&immediate, tmp.path()).unwrap();
        let generation: ExecGeneration =
            serde_json::from_str(&fs::read_to_string(backend.pid_path(id)).unwrap()).unwrap();

        let mut observation = backend.observe_generation(id).unwrap();
        for _ in 0..50 {
            if matches!(observation, ExecGenerationObservation::Exited { .. }) {
                break;
            }
            sleep(Duration::from_millis(20));
            observation = backend.observe_generation(id).unwrap();
        }
        assert!(
            matches!(
                observation,
                ExecGenerationObservation::Exited { pid, .. } if pid == generation.pid
            ),
            "owned child did not converge to exited after reap: {observation:?}"
        );
        assert!(
            !process_alive(generation.pid as i32),
            "owned child remained visible after strict observation"
        );
        backend.remove(id).unwrap();
    }

    #[test]
    fn non_child_generation_mismatch_is_never_reaped_or_signaled() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let catalog = tmp.path().join("catalog");
        fs::create_dir_all(&catalog).unwrap();
        let backend = ExecBackend::new(state, catalog);
        let pid_path = tmp.path().join("foreign.pid");
        let command = format!(
            "sleep 30 </dev/null >/dev/null 2>&1 & printf '%s' \"$!\" > {}",
            pid_path.display()
        );
        assert!(
            std::process::Command::new("sh")
                .args(["-c", &command])
                .status()
                .unwrap()
                .success()
        );
        let pid = fs::read_to_string(&pid_path)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let actual_start = process_start_time_ticks(pid as i32).unwrap();
        let id = "host.test.foreign";
        let created_at = rfc3339_utc(SystemTime::now()).unwrap();
        let mismatched_start = actual_start.saturating_add(1);
        let generation = ExecGeneration {
            schema: EXEC_GENERATION_SCHEMA.to_string(),
            pid,
            generation_id: generation_id(id, pid, &created_at, mismatched_start),
            created_at,
            start_time_ticks: mismatched_start,
        };
        backend.publish_generation(id, &generation).unwrap();

        assert!(matches!(
            backend.observe_generation(id).unwrap(),
            ExecGenerationObservation::Indeterminate {
                alive_for_reconcile: true,
                ..
            }
        ));
        assert!(
            backend
                .kill(id)
                .unwrap_err()
                .to_string()
                .contains("indeterminate")
        );
        assert!(
            process_alive(pid as i32),
            "foreign process was reaped or signaled"
        );

        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        for _ in 0..50 {
            if !process_alive(pid as i32) {
                break;
            }
            sleep(Duration::from_millis(20));
        }
        assert!(
            !process_alive(pid as i32),
            "foreign-process fixture left residue after exact cleanup"
        );
        backend.remove(id).unwrap();
    }

    #[test]
    fn strict_start_token_mismatch_is_indeterminate_and_never_signaled() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog");
        fs::create_dir_all(&catalog).unwrap();
        let backend = ExecBackend::new(tmp.path().join("state"), catalog);
        let id = "host.test.reused";
        backend.spawn(&target(id), tmp.path()).unwrap();

        let path = backend.pid_path(id);
        let mut generation: ExecGeneration =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let actual_pid = generation.pid;
        generation.start_time_ticks = generation.start_time_ticks.saturating_add(1);
        generation.generation_id = generation_id(
            id,
            generation.pid,
            &generation.created_at,
            generation.start_time_ticks,
        );
        backend.publish_generation(id, &generation).unwrap();

        assert!(matches!(
            backend.observe_generation(id).unwrap(),
            ExecGenerationObservation::Indeterminate {
                alive_for_reconcile: true,
                ..
            }
        ));
        assert!(
            backend
                .kill(id)
                .unwrap_err()
                .to_string()
                .contains("indeterminate")
        );
        assert!(process_alive(actual_pid as i32));

        unsafe {
            libc::kill(-(actual_pid as i32), libc::SIGKILL);
        }
        let mut exited = false;
        for _ in 0..50 {
            if matches!(
                backend.observe_generation(id).unwrap(),
                ExecGenerationObservation::Exited { .. }
            ) {
                exited = true;
                break;
            }
            sleep(Duration::from_millis(20));
        }
        assert!(exited, "owned mismatch fixture did not reap after SIGKILL");
        backend.remove(id).unwrap();
    }

    #[test]
    fn malformed_strict_record_is_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let backend = ExecBackend::new(state, tmp.path().join("catalog"));
        let id = "host.test.unknown";
        fs::write(
            backend.pid_path(id),
            format!(
                "{{\"schema\":\"{EXEC_GENERATION_SCHEMA}\",\"pid\":{},\
                 \"createdAt\":\"2026-07-31T00:00:00.000Z\",\"startTimeTicks\":1,\
                 \"generationId\":\"sha256:nope\",\"extra\":true}}",
                std::process::id()
            ),
        )
        .unwrap();

        assert!(matches!(
            backend.observe_generation(id).unwrap(),
            ExecGenerationObservation::Indeterminate {
                alive_for_reconcile: true,
                ref reason,
            } if reason.contains("unknown field")
        ));
        assert!(backend.list().unwrap()[0].alive);
    }

    #[test]
    fn publication_failure_reaps_the_unowned_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let backend = ExecBackend::new(state, tmp.path().join("catalog"));
        let id = "host.test.unpublished";
        fs::create_dir(backend.pid_path(id)).unwrap();

        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        let error = backend.record_spawned_generation(id, pid).unwrap_err();
        assert!(
            error.to_string().contains("publishing exec generation"),
            "{error:#}"
        );
        assert!(
            !process_alive(pid as i32),
            "publication failure leaked process {pid}"
        );
        drop(child);
    }

    #[test]
    fn legacy_plain_pid_is_observed_without_rewriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let backend = ExecBackend::new(state, tmp.path().join("catalog"));
        let id = "host.test.legacy";
        let pid = std::process::id();
        // Leave an unambiguous interval between process creation and record publication.
        sleep(Duration::from_millis(20));
        let legacy = pid.to_string();
        fs::write(backend.pid_path(id), &legacy).unwrap();
        let before =
            RecordEvidence::from_metadata(&fs::symlink_metadata(backend.pid_path(id)).unwrap());

        assert!(matches!(
            backend.observe_generation_optional(id).unwrap(),
            Some(ExecGenerationObservation::Running {
                pid: observed,
                ref created_at,
                ref generation_id,
                ..
            }) if observed == pid
                && crate::task_inventory::is_rfc3339_utc_millis(created_at)
                && generation_id.starts_with("sha256:")
        ));
        assert_eq!(
            fs::read_to_string(backend.pid_path(id)).unwrap(),
            legacy,
            "observation rewrote the legacy state record"
        );
        assert_eq!(
            RecordEvidence::from_metadata(&fs::symlink_metadata(backend.pid_path(id)).unwrap()),
            before,
            "observation changed legacy record identity or write metadata"
        );
    }

    #[test]
    fn absent_record_is_none_and_does_not_create_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("missing-state");
        let backend = ExecBackend::new(state.clone(), tmp.path().join("catalog"));
        assert_eq!(
            backend
                .observe_generation_optional("host.test.absent")
                .unwrap(),
            None
        );
        assert!(!state.exists());
    }

    #[test]
    fn malformed_and_dead_legacy_records_are_indeterminate() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let backend = ExecBackend::new(state, tmp.path().join("catalog"));
        fs::write(backend.pid_path("host.test.bad"), "not-a-pid").unwrap();
        fs::write(backend.pid_path("host.test.dead"), "2000000000").unwrap();

        assert!(matches!(
            backend
                .observe_generation_optional("host.test.bad")
                .unwrap(),
            Some(ExecGenerationObservation::Indeterminate { reason, .. })
                if reason.contains("parsing legacy pid record")
        ));
        assert!(matches!(
            backend
                .observe_generation_optional("host.test.dead")
                .unwrap(),
            Some(ExecGenerationObservation::Indeterminate { reason, .. })
                if reason.contains("not a live process")
        ));
    }

    #[test]
    fn record_older_than_current_process_generation_cannot_be_promoted() {
        let pid = std::process::id() as i32;
        let start = process_start_time_ticks(pid).unwrap();
        assert!(
            !legacy_pid_predates_record(start, UNIX_EPOCH + Duration::from_secs(1)).unwrap(),
            "an ancient record must not identify the current PID generation"
        );
    }

    #[test]
    fn truncate_in_place_during_observation_is_indeterminate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.pid");
        sleep(Duration::from_millis(20));
        fs::write(&path, std::process::id().to_string()).unwrap();
        let record = open_legacy_pid_record(&path).unwrap();

        let observation = observe_open_legacy_generation("host.test.exec", &path, record, || {
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
        });
        let ExecGenerationObservation::Indeterminate { reason, .. } = observation else {
            panic!("truncate-in-place was promoted: {observation:?}");
        };
        assert!(
            reason.contains("record") && reason.contains("changed"),
            "{reason}"
        );
    }

    #[test]
    fn atomic_replace_during_observation_is_indeterminate_even_with_same_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.pid");
        let replacement = tmp.path().join("replacement.pid");
        let raw = std::process::id().to_string();
        sleep(Duration::from_millis(20));
        fs::write(&path, &raw).unwrap();
        let record = open_legacy_pid_record(&path).unwrap();

        let observation = observe_open_legacy_generation("host.test.exec", &path, record, || {
            fs::write(&replacement, &raw).unwrap();
            fs::rename(&replacement, &path).unwrap();
        });
        let ExecGenerationObservation::Indeterminate { reason, .. } = observation else {
            panic!("atomic replacement was promoted: {observation:?}");
        };
        assert!(
            reason.contains("record") && reason.contains("changed"),
            "{reason}"
        );
    }
}
