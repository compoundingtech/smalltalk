use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use pty_client::{PeekScreenOptions, SendOptions, StopError};
use pty_core::registry::SessionInfo;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

const SPAWN_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PTY_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PtySpawnTimeoutPhase {
    Lock,
    Publication,
}

#[derive(Debug)]
pub struct PtySpawnTimeout {
    pub runtime_id: String,
    pub phase: PtySpawnTimeoutPhase,
    pub timeout: Duration,
}

impl fmt::Display for PtySpawnTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "timed out after {}ms waiting for PTY `{}` spawn {}",
            self.timeout.as_millis(),
            self.runtime_id,
            match self.phase {
                PtySpawnTimeoutPhase::Lock => "lock",
                PtySpawnTimeoutPhase::Publication => "publication",
            }
        )
    }
}

impl std::error::Error for PtySpawnTimeout {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Launch {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PtyObservation {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStats {
    name: String,
    process: PtyStatsProcess,
    daemon: PtyStatsDaemon,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStatsProcess {
    alive: bool,
    pid: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PtyStatsDaemon {
    pid: i32,
}

#[derive(Clone)]
pub struct PtyRuntime {
    binary: String,
    root: PathBuf,
    spawn_timeout: Duration,
    command_timeout: Duration,
    command_environment: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for PtyRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtyRuntime")
            .field("binary", &self.binary)
            .field("root", &self.root)
            .field("spawn_timeout", &self.spawn_timeout)
            .finish_non_exhaustive()
    }
}

impl PtyRuntime {
    pub fn new(root: PathBuf) -> Self {
        crate::warn_if_degraded("st3");
        Self {
            binary: "pty".into(),
            root,
            spawn_timeout: SPAWN_PUBLICATION_TIMEOUT,
            command_timeout: PTY_COMMAND_TIMEOUT,
            command_environment: None,
        }
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    /// Environment for the PTY CLI itself, in addition to the target's explicit --env.
    pub fn with_environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.command_environment = Some(environment);
        self
    }

    #[cfg(test)]
    fn with_spawn_timeout(mut self, timeout: Duration) -> Self {
        self.spawn_timeout = timeout;
        self
    }

    #[cfg(test)]
    fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub fn snapshot(&self) -> Result<Vec<PtyObservation>> {
        self.sessions()?.into_iter().map(observation).collect()
    }

    /// One bounded read of the registry. `pty_client` lists an unreadable root as empty, but the
    /// reconciler reads an empty snapshot as every PTY being gone, so that is an error here. A
    /// root that does not exist yet holds no PTYs.
    fn sessions(&self) -> Result<Vec<SessionInfo>> {
        if let Err(error) = std::fs::read_dir(&self.root) {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(Vec::new());
            }
            return Err(error).with_context(|| format!("list PTYs in {}", self.root.display()));
        }
        Ok(
            pty_client::list::list(&self.root, &pty_client::list::ListOptions::default())
                .into_iter()
                .map(|listed| listed.info)
                .collect(),
        )
    }

    fn session(&self, id: &str) -> Result<SessionInfo> {
        self.sessions()?
            .into_iter()
            .find(|session| session.name == id)
            .with_context(|| format!("PTY `{id}` is not present"))
    }

    pub fn spawn(
        &self,
        id: &str,
        launch: &Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        display_name: Option<&str>,
        tags: &BTreeMap<String, String>,
    ) -> Result<()> {
        self.spawn_guarded(id, launch, cwd, env, display_name, tags, None, None)
    }

    /// Start a cutover only after its predecessor exited. A replay can reuse only the exact
    /// operation's tagged replacement; an unrelated live incarnation is never adopted.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_after(
        &self,
        id: &str,
        launch: &Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        display_name: Option<&str>,
        tags: &BTreeMap<String, String>,
        predecessor: &str,
        operation: &str,
    ) -> Result<()> {
        self.spawn_guarded(
            id,
            launch,
            cwd,
            env,
            display_name,
            tags,
            Some((predecessor, operation)),
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_after_checked(
        &self,
        id: &str,
        launch: &Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        display_name: Option<&str>,
        tags: &BTreeMap<String, String>,
        predecessor: &str,
        operation: &str,
        guard: &dyn Fn() -> Result<()>,
    ) -> Result<()> {
        self.spawn_guarded(
            id,
            launch,
            cwd,
            env,
            display_name,
            tags,
            Some((predecessor, operation)),
            Some(guard),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn spawn_guarded(
        &self,
        id: &str,
        launch: &Launch,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        display_name: Option<&str>,
        tags: &BTreeMap<String, String>,
        cutover: Option<(&str, &str)>,
        guard: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<()> {
        let _spawn_lock = self.acquire_spawn_lock(id)?;
        let fence = self.spawn_state_path(id, "pending");
        let mut before = self
            .snapshot()?
            .into_iter()
            .find(|observation| observation.name == id);
        if fence.is_file() {
            let previous = std::fs::read_to_string(&fence)
                .with_context(|| format!("read PTY publication fence {}", fence.display()))?;
            self.wait_for_publication(id, (!previous.is_empty()).then_some(previous.as_str()))?;
            std::fs::remove_file(&fence)
                .with_context(|| format!("clear PTY publication fence {}", fence.display()))?;
            before = self
                .snapshot()?
                .into_iter()
                .find(|observation| observation.name == id);
        }
        if let Some((predecessor, operation)) = cutover
            && let Some(observed) = before.as_ref() {
                anyhow::ensure!(
                    observed.status != "unknown",
                    "rollout cannot spawn while the PTY identity is unknown"
                );
                if observation_is_live(observed) {
                    anyhow::ensure!(
                        observed.tags.get("st3.rollout").map(String::as_str) == Some(operation)
                            && observation_incarnation(observed)
                                .as_deref()
                                .is_some_and(|inc| inc != predecessor),
                        "rollout cannot adopt another live PTY incarnation"
                    );
                } else {
                    let same = observation_incarnation(observed).as_deref() == Some(predecessor)
                        || (observed.pid.is_none()
                            && predecessor.split_once(':').is_some_and(|(_, created)| {
                                observed.created_at.as_deref() == Some(created)
                            }));
                    anyhow::ensure!(same, "rollout predecessor changed before spawn");
                }
        }
        if before.as_ref().is_some_and(observation_is_live) {
            return Ok(());
        }
        // What the previous incarnation left running ends before its replacement starts. Each
        // launch has a scope of its own, so this never reaches the replacement.
        if let Err(error) = self.end_previous_scopes(id, before.as_ref()) {
            eprintln!(
                "st3: WARN what the last incarnation of {id} left running did not end: {error:#}"
            );
        }
        let unit = crate::scope_unit("st3", id);
        if crate::isolation_mode() == crate::Isolation::Scope {
            // The session's record removes itself once its harness exits, so st keeps the name
            // of the scope to end what the harness leaves behind.
            let recorded = self.spawn_state_path(id, "scope");
            std::fs::write(&recorded, &unit)
                .with_context(|| format!("record PTY scope {}", recorded.display()))?;
        }
        if let Some(guard) = guard {
            guard()?;
        }
        let previous_incarnation = before.as_ref().and_then(observation_incarnation);
        std::fs::write(&fence, previous_incarnation.as_deref().unwrap_or_default())
            .with_context(|| format!("write PTY publication fence {}", fence.display()))?;
        let mut arguments = vec![
            OsString::from("run"),
            OsString::from("-d"),
            OsString::from("--force"),
            OsString::from("--id"),
            OsString::from(id),
            OsString::from("--cwd"),
            cwd.as_os_str().to_os_string(),
        ];
        if let Some(display_name) = display_name {
            arguments.extend([OsString::from("--name"), OsString::from(display_name)]);
        } else {
            arguments.push(OsString::from("--no-display-name"));
        }
        let mut terminal_env = env.clone();
        terminal_env
            .entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        for (key, value) in &terminal_env {
            arguments.extend([
                OsString::from("--env"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        let mut effective_tags = tags.clone();
        if let Some((_, operation)) = cutover {
            effective_tags.insert("st3.rollout".into(), operation.into());
        }
        effective_tags.insert(
            "st3.isolation".into(),
            isolation_name(crate::isolation_mode()).into(),
        );
        if crate::isolation_mode() == crate::Isolation::Scope {
            effective_tags.insert("st3.scope-unit".into(), unit.clone());
        }
        for (key, value) in effective_tags {
            arguments.extend([
                OsString::from("--tag"),
                OsString::from(format!("{key}={value}")),
            ]);
        }
        arguments.push(OsString::from("--"));
        arguments.extend(crate::work_prefix().into_iter().map(OsString::from));
        match launch {
            Launch::Shell(source) => {
                arguments.extend([OsString::from("sh"), OsString::from("-c"), source.into()]);
            }
            Launch::Argv(argv) => arguments.extend(argv.iter().map(OsString::from)),
        }
        let argument_refs = arguments
            .iter()
            .map(OsString::as_os_str)
            .collect::<Vec<_>>();
        const ATTEMPTS: u32 = 4;
        let mut last_error = String::new();
        for attempt in 0..ATTEMPTS {
            let mut command =
                crate::wrap_isolated(&unit, std::ffi::OsStr::new(&self.binary), &argument_refs);
            if let Some(environment) = &self.command_environment {
                command.env_clear().envs(environment);
            }
            command.env("PTY_ROOT", &self.root);
            let output = match output_within(command, self.command_timeout) {
                Ok(output) => output,
                Err(error) => {
                    let _ = std::fs::remove_file(&fence);
                    return Err(error);
                }
            };
            if output.status.success() {
                let published = self.wait_for_publication(id, previous_incarnation.as_deref())?;
                std::fs::remove_file(&fence)
                    .with_context(|| format!("clear PTY publication fence {}", fence.display()))?;
                crate::protect_servers(std::slice::from_ref(&published));
                return Ok(());
            }
            last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if !last_error.contains("already in use") || attempt + 1 == ATTEMPTS {
                break;
            }
            if self
                .snapshot()?
                .iter()
                .any(|observation| observation.name == id && observation_is_live(observation))
            {
                let _ = std::fs::remove_file(&fence);
                return Ok(());
            }
            let _ = self.remove(id);
            std::thread::sleep(Duration::from_millis(100 * u64::from(attempt + 1)));
        }
        let _ = std::fs::remove_file(&fence);
        anyhow::bail!("spawn PTY failed: {last_error}")
    }

    fn acquire_spawn_lock(&self, id: &str) -> Result<File> {
        let deadline = Instant::now() + self.spawn_timeout;
        loop {
            if let Some(lock) = self.try_spawn_lock(id)? {
                return Ok(lock);
            }
            if Instant::now() >= deadline {
                return Err(PtySpawnTimeout {
                    runtime_id: id.into(),
                    phase: PtySpawnTimeoutPhase::Lock,
                    timeout: self.spawn_timeout,
                }
                .into());
            }
            std::thread::sleep(SPAWN_POLL_INTERVAL.min(self.spawn_timeout));
        }
    }

    /// The spawn lock of `id`, or none while another holder has it.
    fn try_spawn_lock(&self, id: &str) -> Result<Option<File>> {
        let directory = self.spawn_state_directory();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create PTY spawn-lock directory {}", directory.display()))?;
        let path = self.spawn_state_path(id, "lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open PTY spawn lock {}", path.display()))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(lock));
        }
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            return Ok(None);
        }
        Err(error).with_context(|| format!("lock PTY spawn {}", path.display()))
    }

    fn spawn_state_path(&self, id: &str, extension: &str) -> PathBuf {
        let digest = Sha256::digest(id.as_bytes());
        self.spawn_state_directory()
            .join(format!("{digest:x}.{extension}"))
    }

    fn spawn_state_directory(&self) -> PathBuf {
        let root = self.root.canonicalize().unwrap_or_else(|_| {
            let parent = self
                .root
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .unwrap_or_else(|| PathBuf::from("."));
            self.root
                .file_name()
                .map(|name| parent.join(name))
                .unwrap_or(parent)
        });
        let digest = Sha256::digest(root.as_os_str().as_bytes());
        root.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".st3-pty-spawn-locks")
            .join(format!("{digest:x}"))
    }

    /// Waits for `id` to publish an incarnation other than `previous_incarnation`, and returns it.
    fn wait_for_publication(
        &self,
        id: &str,
        previous_incarnation: Option<&str>,
    ) -> Result<PtyObservation> {
        let deadline = Instant::now() + self.spawn_timeout;
        loop {
            if let Some(published) = self.snapshot()?.into_iter().find(|observation| {
                observation.name == id
                    && observation_incarnation(observation)
                        .as_deref()
                        .is_some_and(|current| Some(current) != previous_incarnation)
            }) {
                return Ok(published);
            }
            if Instant::now() >= deadline {
                return Err(PtySpawnTimeout {
                    runtime_id: id.into(),
                    phase: PtySpawnTimeoutPhase::Publication,
                    timeout: self.spawn_timeout,
                }
                .into());
            }
            std::thread::sleep(SPAWN_POLL_INTERVAL.min(self.spawn_timeout));
        }
    }

    pub fn stop(&self, id: &str) -> Result<()> {
        self.stop_if(id, None)
    }

    pub fn stop_if(&self, id: &str, expected_incarnation: Option<&str>) -> Result<()> {
        let session = self.require_incarnation(id, expected_incarnation)?;
        // Fence the stop on the generation this read saw as well, so a replacement published
        // after the incarnation check is never stopped in its place.
        let generation = session
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.generation.as_deref());
        let stopped =
            pty_client::stop_in(&self.root, id, generation).map_err(|error| match error {
                StopError::GenerationChanged { .. } => {
                    anyhow::anyhow!("PTY `{id}` changed incarnation before the control action")
                }
                error => anyhow::anyhow!("stop PTY failed: {error}"),
            })?;
        // The server has exited, and pty has ended the process tree it measured before the
        // signal. A process that had already left that tree, such as a build whose parent
        // exited, or a process group started after the measurement, is still in the session's
        // work scope, and ends with it.
        if let Some(unit) = work_scope(&session) {
            crate::end_scope(unit, crate::SCOPE_GRACE)?;
        }
        if stopped.verified_empty() {
            return Ok(());
        }
        let running = |pids: &[i32]| {
            pids.iter()
                .copied()
                .filter(|pid| u32::try_from(*pid).is_ok_and(process_runs))
                .collect::<Vec<_>>()
        };
        let survived = running(&stopped.aftermath.survived);
        let escalated = running(stopped.escalated.as_deref().unwrap_or_default());
        let unknown = running(&stopped.aftermath.unknown);
        anyhow::ensure!(
            survived.is_empty() && escalated.is_empty() && unknown.is_empty(),
            "stop PTY failed: the daemon stopped, but processes {survived:?} survived, \
             {escalated:?} survived SIGKILL to their group, and {unknown:?} could not be checked",
        );
        Ok(())
    }

    pub fn kill(&self, id: &str) -> Result<()> {
        self.kill_if(id, None)
    }

    pub fn kill_if(&self, id: &str, expected_incarnation: Option<&str>) -> Result<()> {
        let session = self.require_incarnation(id, expected_incarnation)?;
        // Everything else the harness started ends with it, but the server leaves the work scope
        // first: it outlives the harness to record the exit.
        let unit = work_scope(&session).filter(|unit| {
            server_apart(
                id,
                unit,
                session.pid.and_then(|pid| u32::try_from(pid).ok()),
            )
        });
        self.signal_if(id, expected_incarnation, libc::SIGKILL)?;
        if let Some(unit) = unit {
            crate::end_scope(unit, Duration::ZERO)?;
        }
        Ok(())
    }

    pub fn signal_if(
        &self,
        id: &str,
        expected_incarnation: Option<&str>,
        signal: i32,
    ) -> Result<()> {
        let session = self.require_incarnation(id, expected_incarnation)?;
        let daemon_pid = session
            .pid
            .with_context(|| format!("PTY `{id}` has no process identity"))?;

        // The registry names the supporting daemon, not the process group leader running inside
        // the terminal. Signalling that PID makes the registry disappear while leaving the
        // provider tree alive. Resolve the terminal child through the same daemon and fence it
        // against the registry read before delivering the signal. `pty_client::signal_in` would
        // also prove the daemon by a start token that daemons before pty-rust a2bfa66 never
        // record, so it refuses every session those daemons run.
        let status = pty_core::registry::with_root(&self.root, || {
            pty_client::query_status_json(id, pty_client::STATS_TIMEOUT)
        })
        .map_err(|error| anyhow::anyhow!("read PTY process identity failed: {error}"))?;
        let stats: PtyStats =
            serde_json::from_str(&status).context("parse PTY process identity")?;
        anyhow::ensure!(
            stats.name == id,
            "PTY stats returned `{}` for `{id}`",
            stats.name
        );
        anyhow::ensure!(
            stats.daemon.pid == daemon_pid,
            "PTY `{id}` changed incarnation before signal {signal}"
        );
        anyhow::ensure!(
            stats.process.alive,
            "PTY `{id}` has no live terminal process"
        );
        let pid = stats
            .process
            .pid
            .with_context(|| format!("PTY `{id}` has no terminal process identity"))?;
        // kill(2) reads 0, 1, a negative pid, and this process's own group as more than one
        // program.
        anyhow::ensure!(
            pid > 1 && pid != unsafe { libc::getpgrp() },
            "PTY `{id}` reported the process identity {pid}, which cannot be signalled alone"
        );
        let group = unsafe { libc::kill(-pid, signal) };
        if group != 0 {
            let direct = unsafe { libc::kill(pid, signal) };
            if direct != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).with_context(|| format!("signal {signal} to PTY process"));
                }
            }
        }
        Ok(())
    }

    fn require_incarnation(
        &self,
        id: &str,
        expected_incarnation: Option<&str>,
    ) -> Result<SessionInfo> {
        let session = self.session(id)?;
        if expected_incarnation
            .is_some_and(|expected| session_incarnation(&session).as_deref() != Some(expected))
        {
            anyhow::bail!("PTY `{id}` changed incarnation before the control action");
        }
        Ok(session)
    }

    /// Ends what an ended session's harness left running in its work scope: a build whose parent
    /// exited, a test in a process group of its own. It works after the session's record has
    /// removed itself. A session whose harness still runs keeps everything; [`Self::stop_if`]
    /// ends it.
    pub fn end_leftovers(&self, id: &str) -> Result<()> {
        // A launch under way ends its predecessor's scope itself, and its own is no leftover.
        let Some(_lock) = self.try_spawn_lock(id)? else {
            return Ok(());
        };
        let session = self
            .sessions()?
            .into_iter()
            .find(|session| session.name == id)
            .map(observation)
            .transpose()?;
        // A session the registry cannot rule out as running keeps its scope.
        if session
            .as_ref()
            .is_some_and(|session| matches!(session.status.as_str(), "running" | "unknown"))
        {
            return Ok(());
        }
        self.end_previous_scopes(id, session.as_ref())
    }

    /// [`Self::end_leftovers`] on a background worker, when the last launch of `id` left a scope
    /// to end. Otherwise it costs one file lookup.
    pub fn end_leftovers_later(&self, id: &str) {
        if !self.spawn_state_path(id, "scope").is_file() {
            return;
        }
        let (runtime, id) = (self.clone(), id.to_owned());
        crate::isolate::end_later(format!("pty {} {id}", self.root.display()), move || {
            runtime
                .end_leftovers(&id)
                .with_context(|| format!("end what PTY `{id}` left running"))
        });
    }

    /// Ends the work scopes of `id`'s last launch: the one spawn recorded and the one `ended`,
    /// its session if the record is still there, names. Then it forgets the recorded one. The
    /// caller holds the spawn lock.
    fn end_previous_scopes(&self, id: &str, ended: Option<&PtyObservation>) -> Result<()> {
        let recorded = self.spawn_state_path(id, "scope");
        let mut units = BTreeSet::new();
        match std::fs::read_to_string(&recorded) {
            Ok(unit) => {
                units.insert(unit.trim().to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read PTY scope {}", recorded.display()));
            }
        }
        units.extend(
            ended
                .and_then(|session| session.tags.get("st3.scope-unit"))
                .cloned(),
        );
        // A server that wrote its exit record can still be shutting down.
        let server = ended.and_then(|session| session.pid);
        let mut done = true;
        for unit in &units {
            if server_apart(id, unit, server) {
                crate::end_scope(unit, crate::SCOPE_GRACE)?;
            } else {
                done = false;
            }
        }
        if done {
            match std::fs::remove_file(&recorded) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("forget PTY scope {}", recorded.display()));
                }
            }
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        // The record is the last place that names the session's work scope.
        self.end_leftovers(id)?;
        pty_client::remove_in(&self.root, id)
            .map_err(|error| anyhow::anyhow!("remove PTY failed: {error}"))
    }

    pub fn attach(&self, id: &str) -> Result<()> {
        let mut command = Command::new(&self.binary);
        if let Some(environment) = &self.command_environment {
            command.env_clear().envs(environment);
        }
        let status = command
            .env("PTY_ROOT", &self.root)
            .args(["attach", id])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        anyhow::ensure!(status.success(), "PTY attach failed with {status}");
        Ok(())
    }

    pub fn send_line(&self, id: &str, text: &str) -> Result<()> {
        self.send_line_if(id, text, None)
    }

    pub fn send_line_if(
        &self,
        id: &str,
        text: &str,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        self.send(id, &[text.as_bytes(), b"\r"])
    }

    pub fn send_raw(&self, id: &str, bytes: &[u8]) -> Result<()> {
        self.send_raw_if(id, bytes, None)
    }

    pub fn send_raw_if(
        &self,
        id: &str,
        bytes: &[u8],
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        anyhow::ensure!(
            !bytes.contains(&0),
            "terminal input cannot contain a NUL byte"
        );
        self.send(id, &[bytes])
    }

    pub fn send_key(&self, id: &str, key: &str) -> Result<()> {
        self.send_key_if(id, key, None)
    }

    pub fn send_key_if(
        &self,
        id: &str,
        key: &str,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        self.require_incarnation(id, expected_incarnation)?;
        let key = pty_core::keys::resolve_key(key)
            .map_err(|error| anyhow::anyhow!("send PTY key failed: {error}"))?;
        self.send(id, &[key.as_bytes()])
    }

    /// Each item is one write, with `pty send --seq`'s pause between items so a terminal
    /// program reads a line and its Enter as typing rather than as one paste.
    fn send(&self, id: &str, items: &[&[u8]]) -> Result<()> {
        let options = SendOptions {
            delay_ms: pty_client::DEFAULT_SEQ_DELAY_MS,
            paste: false,
        };
        pty_client::send_in(&self.root, id, items, options)
            .map_err(|error| anyhow::anyhow!("send PTY input failed: {error}"))
    }

    pub fn screen(&self, id: &str) -> Result<String> {
        let options = PeekScreenOptions {
            plain: true,
            full: false,
        };
        let mut screen = pty_client::peek_screen_in(&self.root, id, options)
            .map_err(|error| anyhow::anyhow!("read PTY screen failed: {error}"))?;
        // `pty peek --plain` ended the screen with a newline.
        screen.push('\n');
        Ok(screen)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn binary(&self) -> &str {
        &self.binary
    }
}

/// A wedged PTY command must not stall the reconciler indefinitely.
pub(crate) fn output_within(mut command: Command, timeout: Duration) -> Result<Output> {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(timeout) {
        Ok(output) => Ok(output?),
        Err(_) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            anyhow::bail!(
                "the pty command did not finish within {}s",
                timeout.as_secs()
            )
        }
    }
}

fn isolation_name(mode: crate::Isolation) -> &'static str {
    match mode {
        crate::Isolation::Scope => "scope",
        crate::Isolation::Detached => "detached",
        crate::Isolation::DegradedDetached => "degraded-detached",
    }
}

fn observation_incarnation(observation: &PtyObservation) -> Option<String> {
    match (&observation.pid, &observation.created_at) {
        (Some(pid), Some(created_at)) => Some(format!("{pid}:{created_at}")),
        _ => None,
    }
}

/// The work scope st started `session` in.
fn work_scope(session: &SessionInfo) -> Option<&str> {
    session
        .metadata
        .as_ref()?
        .tags
        .as_ref()?
        .get("st3.scope-unit")
        .map(String::as_str)
}

/// Whether the PTY server `server` of `id`, if it still runs, is outside the work scope `unit`,
/// moving it out first. Ending the scope then leaves the server, which answers attaches and
/// records the exit.
fn server_apart(id: &str, unit: &str, server: Option<u32>) -> bool {
    server
        .filter(|pid| process_runs(*pid))
        .is_none_or(|server| crate::priority::keep_server_apart(id, server, unit))
}

/// Whether `pid` names a process that has not exited. A zombie has exited.
fn process_runs(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    if cfg!(target_os = "linux") {
        return std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, tail)| !tail.starts_with('Z'))
        });
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn observation_is_live(observation: &PtyObservation) -> bool {
    if observation.status != "running" {
        return false;
    }
    observation.pid.is_some_and(|pid| {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        let result = unsafe { libc::kill(pid as i32, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    })
}

/// The registry entry as `pty list --json` printed it.
fn observation(session: SessionInfo) -> Result<PtyObservation> {
    // The old CLI snapshot kept a named but malformed record visible as unknown. Preserve that
    // failure mode so one bad process identity cannot make every other PTY disappear.
    if session.pid.is_some_and(|pid| pid < 0) {
        return Ok(PtyObservation {
            name: session.name,
            status: "unknown".into(),
            exit_code: None,
            pid: None,
            created_at: None,
            display_name: None,
            tags: BTreeMap::new(),
        });
    }
    let pid = session
        .pid
        .map(|pid| {
            u32::try_from(pid).map_err(|_| {
                anyhow::anyhow!(
                    "parse the atomic PTY snapshot: PTY `{}` has the process identity {pid}",
                    session.name
                )
            })
        })
        .transpose()?;
    let (created_at, exit_code, display_name, tags) = match session.metadata {
        Some(metadata) => (
            Some(metadata.created_at),
            metadata.exit_code.map(i64::from),
            metadata.display_name.filter(|name| !name.is_empty()),
            metadata
                .tags
                .map(|tags| tags.into_iter().collect())
                .unwrap_or_default(),
        ),
        None => (None, None, None, BTreeMap::new()),
    };
    Ok(PtyObservation {
        name: session.name,
        status: session.status.as_str().into(),
        exit_code,
        pid,
        created_at,
        display_name,
        tags,
    })
}

/// The same incarnation [`observation_incarnation`] derives, read from the registry entry.
fn session_incarnation(session: &SessionInfo) -> Option<String> {
    Some(format!(
        "{}:{}",
        session.pid?,
        session.metadata.as_ref()?.created_at
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use pty_core::protocol::{MessageType, PacketReader, encode_screen, encode_status_response};

    fn fake_executable(root: &Path, name: &str, body: &str) -> PathBuf {
        let source = root.join(format!("{name}.source"));
        let binary = root.join(name);
        fs::write(&source, body).unwrap();
        // A parallel test can fork while this process owns a writable file.
        // Let install create the executable so no child inherits that writer.
        let output = Command::new("install")
            .args(["-m", "700"])
            .arg(&source)
            .arg(&binary)
            .output()
            .unwrap();
        assert!(output.status.success(), "install failed: {output:?}");
        binary
    }

    /// A fake `pty` whose `run` records its arguments and launch count, then runs `on_run`, which
    /// can call `publish CREATED_AT` to publish `work` into `$PTY_ROOT` the way a daemon does: a
    /// socket entry, a pid file naming the test process (so it reads as alive), and a record.
    fn fake_pty(root: &Path, name: &str, on_run: &str) -> PathBuf {
        let binary = fake_executable(
            root,
            name,
            &format!(
                r#"#!/bin/sh
publish() {{
  mkdir -p "$PTY_ROOT"
  test -e "$PTY_ROOT/work.sock" || : > "$PTY_ROOT/work.sock"
  cat "$0.pid" > "$PTY_ROOT/work.pid"
  printf '{{"createdAt":"%s"}}' "$1" > "$PTY_ROOT/work.json"
}}
if [ "$1" = run ]; then
  printf '%s\n' "$@" > "$0.args"
  count=0
  test ! -f "$0.count" || count="$(cat "$0.count")"
  count=$((count + 1))
  printf '%s\n' "$count" > "$0.count"
{on_run}
fi
exit 0
"#
            ),
        );
        fs::write(binary.with_extension("pid"), std::process::id().to_string()).unwrap();
        binary
    }

    fn write_record(registry: &Path, name: &str, record: serde_json::Value) {
        fs::create_dir_all(registry).unwrap();
        fs::write(registry.join(format!("{name}.json")), record.to_string()).unwrap();
    }

    /// A live session's pid file, naming `pid` as its daemon.
    fn write_pid(registry: &Path, name: &str, pid: u32) {
        fs::write(registry.join(format!("{name}.pid")), pid.to_string()).unwrap();
    }

    #[test]
    fn rollout_spawn_refuses_an_unrelated_live_incarnation_and_reuses_only_its_own_marker() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        let binary = fake_pty(root.path(), "rollout-pty", "  publish replacement");
        let runtime = PtyRuntime::new(registry.clone()).with_binary(binary.to_string_lossy());
        for marker in ["someone-else", "cutover-one"] {
            write_record(
                &registry,
                "work",
                serde_json::json!({"createdAt":"replacement","tags":{"st3.rollout":marker}}),
            );
            write_pid(&registry, "work", std::process::id());
            fs::write(registry.join("work.sock"), "").unwrap();
            let result = runtime.spawn_after(
                "work",
                &Launch::Argv(vec!["true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
                "42:original",
                "cutover-one",
            );
            assert_eq!(result.is_ok(), marker == "cutover-one");
            assert!(
                !binary.with_extension("args").exists(),
                "a replay must not execute a second spawn"
            );
        }
    }

    #[test]
    fn rollout_spawn_rechecks_its_source_guard_inside_the_spawn_lock() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "rollout-source", "  publish replacement");
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());
        let refusal = runtime.spawn_after_checked(
            "work",
            &Launch::Argv(vec!["true".into()]),
            root.path(),
            &BTreeMap::new(),
            None,
            &BTreeMap::new(),
            "42:original",
            "cutover-one",
            &|| anyhow::bail!("source superseded"),
        );
        assert!(
            refusal
                .unwrap_err()
                .to_string()
                .contains("source superseded")
        );
        assert!(!binary.with_extension("args").exists());
    }

    /// A daemon socket for `name` that answers STATUS with `status` and PEEK with `screen`, and
    /// records the payload of every DATA packet it reads.
    fn fake_daemon(
        registry: &Path,
        name: &str,
        status: String,
        screen: &'static [u8],
    ) -> Arc<Mutex<Vec<Vec<u8>>>> {
        fs::create_dir_all(registry).unwrap();
        let listener = UnixListener::bind(registry.join(format!("{name}.sock"))).unwrap();
        let data = Arc::new(Mutex::new(Vec::new()));
        let recorded = data.clone();
        std::thread::spawn(move || {
            for mut socket in listener.incoming().flatten() {
                let mut reader = PacketReader::new();
                let mut buffer = [0; 4096];
                while let Ok(read) = socket.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    for packet in reader.feed(&buffer[..read]).unwrap() {
                        match packet.type_ {
                            MessageType::Status => {
                                let _ = socket.write_all(&encode_status_response(&status));
                            }
                            MessageType::Peek => {
                                let _ = socket.write_all(&encode_screen(screen));
                            }
                            MessageType::Data => recorded.lock().unwrap().push(packet.payload),
                            _ => {}
                        }
                    }
                }
            }
        });
        data
    }

    fn spawn_work(runtime: &PtyRuntime, cwd: &Path, env: &BTreeMap<String, String>) -> Result<()> {
        runtime.spawn(
            "work",
            &Launch::Argv(vec!["true".into()]),
            cwd,
            env,
            None,
            &BTreeMap::new(),
        )
    }

    #[test]
    fn a_pty_spawn_that_never_answers_times_out() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-hung-run", "  exec sleep 60");
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_command_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let error = spawn_work(&runtime, root.path(), &BTreeMap::new()).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.to_string().contains("did not finish"), "{error:#}");
    }

    #[test]
    fn the_snapshot_reads_the_registry_the_way_pty_list_printed_it() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        fake_daemon(&registry, "live", "{}".into(), b"");
        write_pid(&registry, "live", std::process::id());
        write_record(
            &registry,
            "live",
            serde_json::json!({
                "createdAt": "2026-09-28T00:00:00.000Z",
                "displayName": "Live",
                "tags": { "st3.subject": "pty/live" },
            }),
        );
        write_record(
            &registry,
            "done",
            serde_json::json!({
                "createdAt": "2026-09-27T00:00:00.000Z",
                "displayName": "",
                "exitCode": 3,
                "exitedAt": "2026-09-27T01:00:00.000Z",
            }),
        );

        let snapshot = PtyRuntime::new(registry).snapshot().unwrap();

        assert_eq!(
            snapshot,
            [
                PtyObservation {
                    name: "done".into(),
                    status: "exited".into(),
                    exit_code: Some(3),
                    pid: None,
                    created_at: Some("2026-09-27T00:00:00.000Z".into()),
                    display_name: None,
                    tags: BTreeMap::new(),
                },
                PtyObservation {
                    name: "live".into(),
                    status: "running".into(),
                    exit_code: None,
                    pid: Some(std::process::id()),
                    created_at: Some("2026-09-28T00:00:00.000Z".into()),
                    display_name: Some("Live".into()),
                    tags: BTreeMap::from([("st3.subject".into(), "pty/live".into())]),
                },
            ]
        );
    }

    #[test]
    fn a_named_record_with_an_invalid_pid_stays_visible_as_unknown() {
        let record = SessionInfo {
            name: "bad".into(),
            socket_path: PathBuf::from("bad.sock"),
            pid: Some(-1),
            status: pty_core::registry::SessionStatus::Running,
            metadata: None,
        };
        let observed = observation(record).unwrap();
        assert_eq!(observed.name, "bad");
        assert_eq!(observed.status, "unknown");
        assert_eq!(observed.pid, None);
    }

    #[test]
    fn a_missing_registry_is_empty_but_an_unreadable_one_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        assert!(
            PtyRuntime::new(registry.clone())
                .snapshot()
                .unwrap()
                .is_empty()
        );

        // An empty snapshot reads as every PTY being gone, so a registry this process cannot read
        // must fail the snapshot instead.
        fs::write(&registry, b"not a directory").unwrap();
        let error = PtyRuntime::new(registry).snapshot().unwrap_err();
        assert!(error.to_string().starts_with("list PTYs in "), "{error:#}");
    }

    #[test]
    fn terminal_input_checks_the_expected_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        let data = fake_daemon(&registry, "work", "{}".into(), b"");
        write_pid(&registry, "work", std::process::id());
        write_record(&registry, "work", serde_json::json!({ "createdAt": "now" }));
        let runtime = PtyRuntime::new(registry);
        let incarnation = format!("{}:now", std::process::id());

        runtime
            .send_line_if("work", "hello", Some(&incarnation))
            .unwrap();
        runtime
            .send_raw_if("work", b"--bytes", Some(&incarnation))
            .unwrap();
        runtime
            .send_key_if("work", "escape", Some(&incarnation))
            .unwrap();
        let error = runtime
            .send_key_if("work", "escape", Some("41:old"))
            .unwrap_err();
        assert!(error.to_string().contains("changed incarnation"));

        assert_eq!(
            *data.lock().unwrap(),
            [
                b"hello".to_vec(),
                b"\r".to_vec(),
                b"--bytes".to_vec(),
                b"\x1b".to_vec()
            ]
        );
    }

    #[test]
    fn the_screen_is_the_plain_peek_text() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        fake_daemon(&registry, "work", "{}".into(), b"line one\nline two");
        write_pid(&registry, "work", std::process::id());
        write_record(&registry, "work", serde_json::json!({ "createdAt": "now" }));

        let screen = PtyRuntime::new(registry).screen("work").unwrap();

        assert_eq!(screen, "line one\nline two\n");
    }

    #[test]
    fn a_stop_for_a_replaced_session_stops_nothing() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        fake_daemon(&registry, "work", "{}".into(), b"");
        write_pid(&registry, "work", std::process::id());
        write_record(&registry, "work", serde_json::json!({ "createdAt": "new" }));

        let error = PtyRuntime::new(registry)
            .stop_if("work", Some(&format!("{}:old", std::process::id())))
            .unwrap_err();

        assert!(
            error.to_string().contains("changed incarnation"),
            "{error:#}"
        );
    }

    #[test]
    fn terminal_signal_targets_the_terminal_process_group_not_the_daemon() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        let mut command = Command::new("sh");
        command.args(["-c", "trap 'exit 0' HUP; while :; do sleep 1; done"]);
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let daemon = std::process::id();
        fake_daemon(
            &registry,
            "work",
            format!(
                r#"{{"name":"work","process":{{"alive":true,"pid":{}}},"daemon":{{"pid":{daemon}}}}}"#,
                child.id()
            ),
            b"",
        );
        write_pid(&registry, "work", daemon);
        write_record(&registry, "work", serde_json::json!({ "createdAt": "now" }));
        let runtime = PtyRuntime::new(registry);

        runtime
            .signal_if("work", Some(&format!("{daemon}:now")), libc::SIGHUP)
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "terminal process group survived SIGHUP"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_signal_refuses_a_process_identity_kill_would_read_as_a_group() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        let daemon = std::process::id();
        fake_daemon(
            &registry,
            "work",
            format!(
                r#"{{"name":"work","process":{{"alive":true,"pid":1}},"daemon":{{"pid":{daemon}}}}}"#
            ),
            b"",
        );
        write_pid(&registry, "work", daemon);
        write_record(&registry, "work", serde_json::json!({ "createdAt": "now" }));

        let error = PtyRuntime::new(registry)
            .signal_if("work", None, 0)
            .unwrap_err();

        assert!(
            error.to_string().contains("cannot be signalled alone"),
            "{error:#}"
        );
    }

    #[test]
    fn spawn_records_the_shared_isolation_mode() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-spawn", "  publish new");
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());
        assert!(!runtime.spawn_state_directory().starts_with(runtime.root()));
        runtime
            .spawn(
                "work",
                &Launch::Argv(vec!["sh".into(), "-c".into(), "true".into()]),
                root.path(),
                &BTreeMap::new(),
                None,
                &BTreeMap::new(),
            )
            .unwrap();
        let arguments = fs::read_to_string(binary.with_extension("args")).unwrap();
        assert!(arguments.contains("st3.isolation="));
        assert!(arguments.contains("TERM=xterm-256color"));
        assert!(arguments.contains("--force"));
        if crate::isolation_mode() == crate::Isolation::Scope {
            assert!(arguments.contains("st3.scope-unit=st3-work-"));
        }
    }

    #[test]
    fn spawn_preserves_an_explicit_terminal_type() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-term", "  publish new");
        let runtime =
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy());

        spawn_work(
            &runtime,
            root.path(),
            &BTreeMap::from([("TERM".into(), "screen-256color".into())]),
        )
        .unwrap();

        let arguments = fs::read_to_string(binary.with_extension("args")).unwrap();
        assert!(arguments.contains("TERM=screen-256color"));
        assert!(!arguments.contains("TERM=xterm-256color"));
    }

    #[test]
    fn spawn_reaps_a_recent_session_id_and_retries() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        // The first launch meets the exited record of the previous session and refuses the id.
        write_record(
            &registry,
            "work",
            serde_json::json!({
                "generation": "previous",
                "createdAt": "old",
                "exitCode": 0,
                "exitedAt": "2026-09-27T00:00:00.000Z",
            }),
        );
        let binary = fake_pty(
            root.path(),
            "fake-pty-retry",
            r#"  if [ "$count" -eq 1 ]; then
    printf '%s\n' 'Session id "work" is already in use.' >&2
    exit 1
  fi
  test ! -f "$PTY_ROOT/work.json" || touch "$0.stale"
  publish new"#,
        );
        let runtime = PtyRuntime::new(registry).with_binary(binary.to_string_lossy());

        spawn_work(&runtime, root.path(), &BTreeMap::new()).unwrap();

        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "2"
        );
        assert!(
            !binary.with_extension("stale").exists(),
            "the retry ran before the exited record was removed"
        );
    }

    #[test]
    fn spawn_waits_for_an_exact_new_registry_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        // The previous session exited under the same daemon pid, so only `createdAt` tells the
        // incarnations apart.
        fs::create_dir_all(&registry).unwrap();
        fs::write(registry.join("work.sock"), b"").unwrap();
        write_pid(&registry, "work", std::process::id());
        write_record(
            &registry,
            "work",
            serde_json::json!({
                "createdAt": "old",
                "exitCode": 0,
                "exitedAt": "2026-09-27T00:00:00.000Z",
            }),
        );
        let binary = fake_pty(
            root.path(),
            "fake-pty-delayed-publication",
            r#"  (sleep 0.3; publish new) >/dev/null 2>&1 &"#,
        );
        let runtime = PtyRuntime::new(registry).with_binary(binary.to_string_lossy());
        let started = Instant::now();

        spawn_work(&runtime, root.path(), &BTreeMap::new()).unwrap();

        assert!(started.elapsed() >= Duration::from_millis(300));
        let observation = runtime
            .snapshot()
            .unwrap()
            .into_iter()
            .find(|observation| observation.name == "work")
            .unwrap();
        assert_eq!(observation.created_at.as_deref(), Some("new"));
    }

    #[test]
    fn concurrent_spawns_for_one_runtime_launch_only_once() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-concurrent", "  publish new");
        let runtime = Arc::new(
            PtyRuntime::new(root.path().join("registry")).with_binary(binary.to_string_lossy()),
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let cwd = root.path().to_path_buf();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                spawn_work(&runtime, &cwd, &BTreeMap::new())
            }));
        }
        for thread in threads {
            thread.join().unwrap().unwrap();
        }

        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "1"
        );
    }

    #[test]
    fn publication_timeout_is_typed_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-never-publishes", "");
        let timeout = Duration::from_millis(30);
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(timeout);
        let started = Instant::now();

        let error = spawn_work(&runtime, root.path(), &BTreeMap::new()).unwrap_err();

        let timeout_error = error.downcast_ref::<PtySpawnTimeout>().unwrap();
        assert_eq!(timeout_error.phase, PtySpawnTimeoutPhase::Publication);
        assert_eq!(timeout_error.timeout, timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_unresolved_publication_fences_followup_launches() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_pty(root.path(), "fake-pty-unresolved-publication", "");
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(Duration::from_millis(30));
        let spawn = || spawn_work(&runtime, root.path(), &BTreeMap::new());

        let first = spawn().unwrap_err();
        let second = spawn().unwrap_err();

        assert_eq!(
            first.downcast_ref::<PtySpawnTimeout>().unwrap().phase,
            PtySpawnTimeoutPhase::Publication
        );
        assert_eq!(
            second.downcast_ref::<PtySpawnTimeout>().unwrap().phase,
            PtySpawnTimeoutPhase::Publication
        );
        assert_eq!(
            fs::read_to_string(binary.with_extension("count"))
                .unwrap()
                .trim(),
            "1",
            "the unresolved first launch must fence later callers"
        );
    }

    #[test]
    fn spawn_lock_timeout_is_typed_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        let binary = fake_executable(root.path(), "fake-pty-lock-timeout", "#!/bin/sh\nexit 0\n");
        let timeout = Duration::from_millis(30);
        let runtime = PtyRuntime::new(root.path().join("registry"))
            .with_binary(binary.to_string_lossy())
            .with_spawn_timeout(timeout);
        let _held = runtime.acquire_spawn_lock("work").unwrap();
        let started = Instant::now();

        let error = spawn_work(&runtime, root.path(), &BTreeMap::new()).unwrap_err();

        let timeout_error = error.downcast_ref::<PtySpawnTimeout>().unwrap();
        assert_eq!(timeout_error.phase, PtySpawnTimeoutPhase::Lock);
        assert_eq!(timeout_error.timeout, timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// The installed `pty`, when this host runs sessions in systemd user scopes.
    fn scoped_pty() -> Option<PathBuf> {
        if crate::isolation_mode() != crate::Isolation::Scope {
            return None;
        }
        let environment = BTreeMap::from([("PATH".into(), std::env::var("PATH").ok()?)]);
        crate::resolve_executable("pty", &environment).ok()
    }

    fn cgroup_leaf(pid: u32) -> String {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let path = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap();
        path.rsplit('/').next().unwrap().to_owned()
    }

    fn cgroup_file(pid: u32, file: &str) -> String {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
        let path = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap();
        fs::read_to_string(
            Path::new("/sys/fs/cgroup")
                .join(path.trim_start_matches('/'))
                .join(file),
        )
        .unwrap()
    }

    /// A harness that writes its pid to `pid` and waits.
    fn waiting_harness(pid: &Path) -> Launch {
        Launch::Argv(vec![
            "sh".into(),
            "-c".into(),
            format!("echo $$ > '{}'; exec sleep 60", pid.display()),
        ])
    }

    fn read_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "the harness never wrote its pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Asks st to protect `id`'s server until it runs in its own scope, then checks that the
    /// harness stayed behind in the scope st started the session in.
    fn assert_server_left_its_harness(runtime: &PtyRuntime, id: &str, harness: u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (server, work_unit) = loop {
            let observation = runtime.session(id).map(observation).unwrap().unwrap();
            crate::protect_servers(std::slice::from_ref(&observation));
            let server = observation.pid.unwrap();
            if cgroup_leaf(server) == crate::server_unit(id, server) {
                break (server, observation.tags["st3.scope-unit"].clone());
            }
            assert!(
                Instant::now() < deadline,
                "PTY server {server} stayed in {}",
                cgroup_leaf(server)
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(cgroup_leaf(harness), work_unit);
        assert_eq!(cgroup_file(server, "cpu.weight").trim(), "1000");
        assert_eq!(cgroup_file(harness, "cpu.weight").trim(), "100");
    }

    #[test]
    fn a_spawned_pty_server_runs_apart_from_its_harness() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let runtime = PtyRuntime::new(root.path().join("r")).with_binary(pty.to_string_lossy());
        let pid_file = root.path().join("harness.pid");
        let environment = BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]);
        runtime
            .spawn(
                "protect-spawn",
                &waiting_harness(&pid_file),
                root.path(),
                &environment,
                None,
                &BTreeMap::new(),
            )
            .unwrap();
        let harness = read_pid(&pid_file);

        assert_server_left_its_harness(&runtime, "protect-spawn", harness);
        runtime.stop("protect-spawn").unwrap();
    }

    #[test]
    fn a_pty_server_from_an_older_release_moves_when_observed() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("r");
        let pid_file = root.path().join("harness.pid");
        let Launch::Argv(harness_argv) = waiting_harness(&pid_file) else {
            unreachable!()
        };
        // An older release started the server in the session's scope and left it there.
        let unit = crate::scope_unit("st3", "protect-old");
        let mut arguments = vec![
            "run".to_owned(),
            "-d".into(),
            "--id".into(),
            "protect-old".into(),
            "--tag".into(),
            format!("st3.scope-unit={unit}"),
            "--".into(),
        ];
        arguments.extend(harness_argv);
        let argument_refs = arguments
            .iter()
            .map(std::ffi::OsStr::new)
            .collect::<Vec<_>>();
        let status = crate::wrap_isolated(&unit, pty.as_os_str(), &argument_refs)
            .env("PTY_ROOT", &registry)
            .status()
            .unwrap();
        assert!(status.success());
        let harness = read_pid(&pid_file);
        let runtime = PtyRuntime::new(registry).with_binary(pty.to_string_lossy());
        let server = runtime.session("protect-old").unwrap().pid.unwrap() as u32;
        assert_eq!(cgroup_leaf(server), unit);

        assert_server_left_its_harness(&runtime, "protect-old", harness);
        runtime.stop("protect-old").unwrap();
    }

    /// A harness that writes its pid to `harness`, starts a process in a session of its own whose
    /// parent exits at once, waits for that process to write its pid to `orphan`, then runs
    /// `then`. The process has left the harness's process tree and process groups, the way a
    /// tool's background build does.
    fn orphaning_harness(harness: &Path, orphan: &Path, then: &str) -> Launch {
        let orphan = orphan.display();
        Launch::Argv(vec![
            "sh".into(),
            "-c".into(),
            format!(
                "echo $$ > '{}'; (setsid sh -c 'echo $$ > \"{orphan}\"; exec sleep 600' &); \
                 while [ ! -s '{orphan}' ]; do sleep 0.02; done; {then}",
                harness.display(),
            ),
        ])
    }

    fn assert_gone_soon(pid: u32, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_runs(pid) {
            if Instant::now() >= deadline {
                // Leave nothing behind for the next run.
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                panic!("{what} {pid} outlived its session");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A session a test started. Dropping it stops what still runs, so a failing test leaves
    /// nothing behind.
    struct Started {
        runtime: PtyRuntime,
        id: String,
        root: tempfile::TempDir,
        harness: u32,
        orphan: u32,
    }

    impl Drop for Started {
        fn drop(&mut self) {
            let _ = self.runtime.stop(&self.id);
            let _ = self.runtime.end_leftovers(&self.id);
        }
    }

    impl Started {
        /// Starts `id` with an [`orphaning_harness`] and `tags`.
        fn orphaning(pty: &Path, id: &str, then: &str, tags: &[(&str, &str)]) -> Self {
            let root = tempfile::tempdir().unwrap();
            let runtime = PtyRuntime::new(root.path().join("r")).with_binary(pty.to_string_lossy());
            let (harness, orphan) = (
                root.path().join("harness.pid"),
                root.path().join("orphan.pid"),
            );
            let mut started = Self {
                runtime,
                id: id.into(),
                root,
                harness: 0,
                orphan: 0,
            };
            started.spawn(&orphaning_harness(&harness, &orphan, then), tags);
            started.harness = read_pid(&harness);
            started.orphan = read_pid(&orphan);
            assert!(process_runs(started.orphan));
            assert!(cgroup_leaf(started.orphan).starts_with(&format!("st3-{id}-")));
            started
        }

        fn spawn(&self, launch: &Launch, tags: &[(&str, &str)]) {
            let environment = BTreeMap::from([("PATH".into(), std::env::var("PATH").unwrap())]);
            let tags = tags
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            self.runtime
                .spawn(
                    &self.id,
                    launch,
                    self.root.path(),
                    &environment,
                    None,
                    &tags,
                )
                .unwrap();
        }

        fn observe(&self) -> Option<PtyObservation> {
            self.runtime
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|observation| observation.name == self.id)
        }

        fn incarnation(&self) -> String {
            observation_incarnation(&self.observe().unwrap()).unwrap()
        }

        /// Waits until the session's server has exited. Unless the session is tagged
        /// `keep=true`, its record has removed itself by then.
        fn wait_until_ended(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let observation = self.observe();
                if observation.as_ref().is_none_or(|observation| {
                    observation.status != "running" && !observation.pid.is_some_and(process_runs)
                }) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY `{}` kept running: {observation:?}",
                    self.id
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    #[test]
    fn stopping_a_session_ends_what_its_harness_left_running() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let session = Started::orphaning(&pty, "tree-stop", "exec sleep 600", &[]);

        session
            .runtime
            .stop_if(&session.id, Some(&session.incarnation()))
            .unwrap();

        assert_gone_soon(session.harness, "harness");
        assert_gone_soon(session.orphan, "the harness's background process");
    }

    #[test]
    fn killing_a_session_ends_what_its_harness_left_running() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let session = Started::orphaning(&pty, "tree-kill", "exec sleep 600", &[]);

        session
            .runtime
            .kill_if(&session.id, Some(&session.incarnation()))
            .unwrap();

        assert_gone_soon(session.harness, "harness");
        assert_gone_soon(session.orphan, "the harness's background process");
        session.wait_until_ended();
    }

    #[test]
    fn an_ended_session_ends_what_its_harness_left_running() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let session = Started::orphaning(&pty, "tree-ended", "exit 0", &[]);
        session.wait_until_ended();

        // The reconciler asks this on each pass in which a stopped runtime is not running.
        session.runtime.end_leftovers_later(&session.id);

        assert_gone_soon(session.orphan, "the ended harness's background process");
    }

    #[test]
    fn removing_an_ended_session_ends_what_its_harness_left_running() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let session = Started::orphaning(&pty, "tree-remove", "exit 0", &[("keep", "true")]);
        session.wait_until_ended();

        session.runtime.remove(&session.id).unwrap();

        assert_gone_soon(session.orphan, "the ended harness's background process");
    }

    #[test]
    fn an_ended_sessions_leftovers_end_and_its_replacement_keeps_running() {
        let Some(pty) = scoped_pty() else {
            eprintln!("skipped: no systemd user scopes or no pty binary");
            return;
        };
        let session = Started::orphaning(&pty, "tree-replace", "exit 0", &[]);
        let ended_unit = cgroup_leaf(session.orphan);
        session.wait_until_ended();

        let pid_file = session.root.path().join("replacement.pid");
        session.spawn(&waiting_harness(&pid_file), &[]);
        let replacement = read_pid(&pid_file);

        assert_gone_soon(session.orphan, "the ended harness's background process");
        // Each launch has a scope of its own. Ending the ended one again, as a reconcile pass
        // that looked before the replacement started does, leaves the replacement.
        let replacement_unit = session.observe().unwrap().tags["st3.scope-unit"].clone();
        assert_eq!(cgroup_leaf(replacement), replacement_unit);
        assert_ne!(replacement_unit, ended_unit);
        assert!(crate::end_scope(&ended_unit, crate::SCOPE_GRACE).unwrap());
        session.runtime.end_leftovers(&session.id).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(process_runs(replacement), "the replacement ended");
        session.runtime.stop(&session.id).unwrap();
        assert_gone_soon(replacement, "replacement harness");
    }
}
