use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::Deserialize;
use st2_resource_wasip2::{
    CapabilityContext, CapabilityModule, CapabilityPhase, InterruptionReason,
    InvocationControl as ExecutorInvocationControl, InvocationStore,
};
use wasmtime::component::{HasSelf, Linker};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit/pty-stats",
        world: "pty-stats-provider",
    });
}

use bindings::compoundingtech::st2_pty_stats::pty_stats::{
    Clients, Generation, Host, Lifecycle, Metadata, Modes, Process, ProcessResources,
    PtyStatsError, Runtime, SessionSource, SourceObservation, Tag, Terminal,
};

const IMPORT_NAME: &str = "compoundingtech:st2-pty-stats/pty-stats@0.1.0";
const MAX_STDOUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const SNAPSHOT_DIGEST_BYTES: usize = 32;
const MAX_CACHED_SNAPSHOTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyStatsConfig {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub deadline: Duration,
}

impl PtyStatsConfig {
    pub fn resolve(
        executable: impl AsRef<Path>,
        cwd: impl Into<PathBuf>,
        deadline: Duration,
    ) -> Result<Self, &'static str> {
        if deadline.is_zero() || deadline > Duration::from_secs(60) {
            return Err("PTY control-plane deadline is invalid");
        }
        let executable =
            resolve_executable(executable.as_ref()).ok_or("PTY executable is unavailable")?;
        let cwd = cwd.into();
        if !cwd.is_absolute() {
            return Err("PTY control-plane cwd must be absolute");
        }
        Ok(Self {
            executable,
            cwd,
            deadline,
        })
    }
}

#[derive(Clone)]
pub struct PtyStatsModule {
    config: PtyStatsConfig,
    cache: Arc<Mutex<SnapshotCache>>,
}

impl PtyStatsModule {
    pub fn new(config: PtyStatsConfig) -> Self {
        Self {
            config,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedSource {
    id: String,
    observed_at: String,
    lifecycle: SourceLifecycle,
    generation: Option<SourceGeneration>,
    metadata: Option<SourceMetadata>,
    runtime: Option<SourceRuntime>,
}

impl CachedSource {
    fn absent(id: &str, observed_at: String) -> Self {
        Self {
            id: id.into(),
            observed_at,
            lifecycle: SourceLifecycle::Absent,
            generation: None,
            metadata: None,
            runtime: None,
        }
    }
}

#[derive(Default)]
struct SnapshotCache {
    sources: BTreeMap<[u8; SNAPSHOT_DIGEST_BYTES], CachedSource>,
}

impl SnapshotCache {
    fn get(&self, digest: &[u8; SNAPSHOT_DIGEST_BYTES]) -> Option<&CachedSource> {
        self.sources.get(digest)
    }

    fn insert(&mut self, digest: [u8; SNAPSHOT_DIGEST_BYTES], source: CachedSource) {
        if !self.sources.contains_key(&digest)
            && self.sources.len() >= MAX_CACHED_SNAPSHOTS
            && let Some(evicted) = self.sources.keys().next().copied()
        {
            self.sources.remove(&evicted);
        }
        self.sources.insert(digest, source);
    }
}

pub struct PtyStatsInvocation {
    config: PtyStatsConfig,
    cache: Arc<Mutex<SnapshotCache>>,
    prior_digest: Option<[u8; SNAPSHOT_DIGEST_BYTES]>,
    current_source: Option<CachedSource>,
    deadline: Instant,
    control: Arc<ProcessControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Termination {
    None = 0,
    Cancelled = 1,
    TimedOut = 2,
}

struct ProcessControl {
    termination: AtomicU8,
    child: Mutex<ChildOwnership>,
    #[cfg(not(test))]
    invocation: ExecutorInvocationControl,
    #[cfg(test)]
    invocation: Option<ExecutorInvocationControl>,
}

enum ChildOwnership {
    Pending,
    Live(i32),
    Reaping,
}

impl ProcessControl {
    fn new(invocation: ExecutorInvocationControl) -> Self {
        Self {
            termination: AtomicU8::new(Termination::None as u8),
            child: Mutex::new(ChildOwnership::Pending),
            #[cfg(not(test))]
            invocation,
            #[cfg(test)]
            invocation: Some(invocation),
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            termination: AtomicU8::new(Termination::None as u8),
            child: Mutex::new(ChildOwnership::Pending),
            invocation: None,
        }
    }

    fn termination(&self) -> Termination {
        match self.termination.load(Ordering::Acquire) {
            1 => Termination::Cancelled,
            2 => Termination::TimedOut,
            _ => match self.executor_interruption() {
                Some(InterruptionReason::Cancelled) => Termination::Cancelled,
                Some(InterruptionReason::TimedOut) => Termination::TimedOut,
                None => Termination::None,
            },
        }
    }

    fn executor_interruption(&self) -> Option<InterruptionReason> {
        #[cfg(not(test))]
        {
            self.invocation.interruption_reason()
        }
        #[cfg(test)]
        {
            self.invocation
                .as_ref()
                .and_then(ExecutorInvocationControl::interruption_reason)
        }
    }

    fn install(&self, process_group: i32) {
        let mut ownership = self.child.lock();
        debug_assert!(matches!(*ownership, ChildOwnership::Pending));
        *ownership = ChildOwnership::Live(process_group);
        if self.termination() != Termination::None {
            let _ = kill_process_group(process_group);
        }
    }

    fn terminate(&self, reason: Termination) -> bool {
        let changed = self
            .termination
            .compare_exchange(
                Termination::None as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if changed && let ChildOwnership::Live(process_group) = *self.child.lock() {
            let _ = kill_process_group(process_group);
        }
        changed
    }

    fn synchronize_interruption(&self) -> Termination {
        let reason = self.termination();
        if reason != Termination::None {
            self.terminate(reason);
        }
        reason
    }

    fn wait_and_reap(
        &self,
        child: &mut std::process::Child,
    ) -> std::io::Result<std::process::ExitStatus> {
        wait_without_reaping(child.id())?;
        let mut ownership = self.child.lock();
        debug_assert!(matches!(*ownership, ChildOwnership::Live(_)));
        *ownership = ChildOwnership::Reaping;
        let status = child.wait();
        *ownership = ChildOwnership::Pending;
        status
    }

    fn kill_and_reap(&self, child: &mut std::process::Child) {
        let mut ownership = self.child.lock();
        if let ChildOwnership::Live(process_group) = *ownership {
            let _ = kill_process_group(process_group);
        }
        *ownership = ChildOwnership::Reaping;
        let _ = child.wait();
        *ownership = ChildOwnership::Pending;
    }
}

impl CapabilityModule for PtyStatsModule {
    type Invocation = PtyStatsInvocation;

    fn import_names(&self) -> &'static [&'static str] {
        &[IMPORT_NAME]
    }

    fn add_to_linker(
        &self,
        linker: &mut Linker<InvocationStore<Self::Invocation>>,
    ) -> Result<(), wasmtime::Error> {
        bindings::PtyStatsProvider::add_to_linker::<_, HasSelf<_>>(linker, |state| state)
    }

    fn begin(&self, context: CapabilityContext<'_>) -> Self::Invocation {
        let prior_digest = match context.phase() {
            CapabilityPhase::Describe => None,
            CapabilityPhase::Observe(request) => request
                .prior_digest
                .as_ref()
                .map(|digest| *digest.as_bytes()),
        };
        let deadline = Instant::now() + self.config.deadline;
        PtyStatsInvocation {
            config: self.config.clone(),
            cache: Arc::clone(&self.cache),
            prior_digest,
            current_source: None,
            deadline,
            control: Arc::new(ProcessControl::new(context.control().clone())),
        }
    }
}

impl Host for InvocationStore<PtyStatsInvocation> {
    fn list_session(&mut self, session: String) -> Result<SourceObservation, PtyStatsError> {
        self.capability_mut().list(session)
    }

    fn stats(&mut self, session: String) -> Result<SessionSource, PtyStatsError> {
        self.capability_mut().stats(session)
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), PtyStatsError> {
        self.capability_mut().bind_snapshot(digest)
    }
}

impl PtyStatsInvocation {
    fn list(&mut self, session: String) -> Result<SourceObservation, PtyStatsError> {
        if !valid_session_id(&session) {
            return Err(PtyStatsError::Denied);
        }
        let previous = self
            .prior_digest
            .as_ref()
            .and_then(|digest| self.cache.lock().get(digest).cloned())
            .filter(|source| source.id == session);
        let outcome = self.run(&["list", "--json"])?;
        require_success(&outcome)?;
        let sessions: Vec<ListSession> =
            serde_json::from_slice(&outcome.stdout).map_err(|_| PtyStatsError::Unavailable)?;
        let mut matches = sessions
            .into_iter()
            .filter(|candidate| candidate.name == session);
        let observed_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let current = match matches.next() {
            Some(found) => CachedSource::from(found, observed_at),
            None => CachedSource::absent(&session, observed_at),
        };
        if matches.next().is_some() {
            return Err(PtyStatsError::Unavailable);
        }
        self.current_source = Some(current.clone());
        Ok(SourceObservation {
            current: source_to_wit(&current),
            previous: previous.as_ref().map(source_to_wit),
        })
    }

    fn stats(&mut self, session: String) -> Result<SessionSource, PtyStatsError> {
        if !valid_session_id(&session) {
            return Err(PtyStatsError::Denied);
        }
        let mut current = self
            .current_source
            .take()
            .filter(|source| source.id == session && source.lifecycle == SourceLifecycle::Running)
            .ok_or(PtyStatsError::Denied)?;
        let outcome = self.run(&["stats", "--json", &session])?;
        if !successful(&outcome) {
            if contains_not_found(&outcome.stderr) {
                current = CachedSource::absent(&session, current.observed_at);
                self.current_source = Some(current.clone());
                return Ok(source_to_wit(&current));
            }
            return Err(PtyStatsError::Unavailable);
        }
        let stats: StatsResponse =
            serde_json::from_slice(&outcome.stdout).map_err(|_| PtyStatsError::Unavailable)?;
        if stats.name != session {
            return Err(PtyStatsError::Unavailable);
        }
        current.apply_stats(stats)?;
        self.current_source = Some(current.clone());
        Ok(source_to_wit(&current))
    }

    fn bind_snapshot(&mut self, digest: Vec<u8>) -> Result<(), PtyStatsError> {
        let digest: [u8; SNAPSHOT_DIGEST_BYTES] =
            digest.try_into().map_err(|_| PtyStatsError::Denied)?;
        let source = self
            .current_source
            .take()
            .ok_or(PtyStatsError::Unavailable)?;
        self.cache.lock().insert(digest, source);
        Ok(())
    }

    fn run(&mut self, arguments: &[&str]) -> Result<CommandOutcome, PtyStatsError> {
        match self.control.termination() {
            Termination::Cancelled => return Err(PtyStatsError::Cancelled),
            Termination::TimedOut => return Err(PtyStatsError::DeadlineExceeded),
            Termination::None => {}
        }
        if self.deadline <= Instant::now() {
            self.control.terminate(Termination::TimedOut);
            return Err(PtyStatsError::DeadlineExceeded);
        }
        let mut command = Command::new(&self.config.executable);
        command
            .args(arguments)
            .current_dir(&self.config.cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: this runs in the freshly-forked child before exec and calls only async-signal-safe
        // setpgid. The dedicated process group is the cancellation/reaping boundary.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command.spawn().map_err(|_| PtyStatsError::Unavailable)?;
        self.control.install(child.id() as i32);
        let Some(stdout) = child.stdout.take() else {
            self.control.kill_and_reap(&mut child);
            return Err(PtyStatsError::Unavailable);
        };
        let Some(stderr) = child.stderr.take() else {
            self.control.kill_and_reap(&mut child);
            return Err(PtyStatsError::Unavailable);
        };
        let stdout_reader = match thread::Builder::new()
            .name("st2-pty-stats-stdout".into())
            .spawn(move || drain_bounded(stdout, MAX_STDOUT_BYTES))
        {
            Ok(reader) => reader,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                return Err(PtyStatsError::Unavailable);
            }
        };
        let stderr_reader = match thread::Builder::new()
            .name("st2-pty-stats-stderr".into())
            .spawn(move || drain_bounded(stderr, MAX_STDERR_BYTES))
        {
            Ok(reader) => reader,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                let _ = stdout_reader.join();
                return Err(PtyStatsError::Unavailable);
            }
        };
        let (completed_tx, completed_rx) = mpsc::sync_channel(1);
        let deadline = self.deadline;
        let deadline_control = Arc::clone(&self.control);
        let timer = match thread::Builder::new()
            .name("st2-pty-stats-deadline".into())
            .spawn(move || {
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        deadline_control.terminate(Termination::TimedOut);
                        return;
                    }
                    match completed_rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if deadline_control.synchronize_interruption() != Termination::None {
                        return;
                    }
                }
            }) {
            Ok(timer) => timer,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(PtyStatsError::Unavailable);
            }
        };
        let status = match self.control.wait_and_reap(&mut child) {
            Ok(status) => status,
            Err(_) => {
                self.control.kill_and_reap(&mut child);
                let _ = completed_tx.send(());
                let _ = timer.join();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(PtyStatsError::Unavailable);
            }
        };
        let _ = completed_tx.send(());
        let _ = timer.join();
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| PtyStatsError::Unavailable)??;
        let (stderr, stderr_truncated) = stderr_reader
            .join()
            .map_err(|_| PtyStatsError::Unavailable)??;
        if self.control.termination() == Termination::None && self.deadline <= Instant::now() {
            self.control.terminate(Termination::TimedOut);
        }
        match self.control.termination() {
            Termination::Cancelled => return Err(PtyStatsError::Cancelled),
            Termination::TimedOut => return Err(PtyStatsError::DeadlineExceeded),
            Termination::None => {}
        }
        if stdout_truncated || stderr_truncated {
            return Err(PtyStatsError::ResourceExhausted);
        }
        let exit = status.code().map_or(CommandExit::Signal, CommandExit::Code);
        Ok(CommandOutcome {
            stdout,
            stderr,
            exit,
        })
    }
}

#[derive(Debug)]
struct CommandOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: CommandExit,
}

#[derive(Debug)]
enum CommandExit {
    Code(i32),
    Signal,
}

fn successful(outcome: &CommandOutcome) -> bool {
    matches!(outcome.exit, CommandExit::Code(0))
}

fn require_success(outcome: &CommandOutcome) -> Result<(), PtyStatsError> {
    successful(outcome)
        .then_some(())
        .ok_or(PtyStatsError::Unavailable)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SourceLifecycle {
    Running,
    Exited,
    Vanished,
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum SourceGeneration {
    Number(u64),
    Timestamp(String),
}

#[derive(Debug, Clone)]
struct SourceMetadata {
    display_name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    created_at: Option<String>,
    exit_code: Option<i32>,
    exited_at: Option<String>,
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSession {
    name: String,
    status: SourceLifecycle,
    command: Option<String>,
    cwd: Option<String>,
    created_at: Option<String>,
    exit_code: Option<i32>,
    exited_at: Option<String>,
    tags: Option<BTreeMap<String, String>>,
    display_name: Option<String>,
    generation: Option<SourceGeneration>,
}

impl CachedSource {
    fn from(session: ListSession, observed_at: String) -> Self {
        let generation = session
            .generation
            .clone()
            .or_else(|| session.created_at.clone().map(SourceGeneration::Timestamp));
        Self {
            id: session.name,
            observed_at,
            lifecycle: session.status,
            generation,
            metadata: Some(SourceMetadata {
                display_name: session.display_name,
                command: session.command,
                cwd: session.cwd,
                created_at: session.created_at,
                exit_code: session.exit_code,
                exited_at: session.exited_at,
                tags: session.tags,
            }),
            runtime: None,
        }
    }

    fn apply_stats(&mut self, stats: StatsResponse) -> Result<(), PtyStatsError> {
        let stats_generation = stats
            .generation
            .or_else(|| stats.created_at.map(SourceGeneration::Timestamp));
        self.generation = stats_generation.or_else(|| self.generation.clone());
        match stats.status {
            Some(SourceLifecycle::Exited | SourceLifecycle::Vanished) => {
                self.lifecycle = stats.status.expect("the gone PTY status was present");
                self.runtime = None;
                let metadata = self.metadata.as_mut().ok_or(PtyStatsError::Unavailable)?;
                metadata.exit_code = stats.exit_code.or(metadata.exit_code);
                metadata.exited_at = stats.exited_at.or_else(|| metadata.exited_at.clone());
                metadata.tags = stats.tags.or_else(|| metadata.tags.clone());
            }
            Some(SourceLifecycle::Absent) => {
                *self = Self::absent(&self.id, self.observed_at.clone());
            }
            Some(SourceLifecycle::Running) | None => {
                self.lifecycle = SourceLifecycle::Running;
                self.runtime = Some(SourceRuntime {
                    terminal: stats.terminal.ok_or(PtyStatsError::Unavailable)?,
                    process: stats.process.ok_or(PtyStatsError::Unavailable)?,
                    clients: stats.clients.ok_or(PtyStatsError::Unavailable)?,
                    modes: stats.modes.ok_or(PtyStatsError::Unavailable)?,
                    uptime_seconds: stats.uptime_seconds,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatsResponse {
    name: String,
    status: Option<SourceLifecycle>,
    terminal: Option<SourceTerminal>,
    process: Option<SourceProcess>,
    clients: Option<SourceClients>,
    modes: Option<SourceModes>,
    uptime_seconds: Option<u64>,
    created_at: Option<String>,
    generation: Option<SourceGeneration>,
    exit_code: Option<i32>,
    exited_at: Option<String>,
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone)]
struct SourceRuntime {
    terminal: SourceTerminal,
    process: SourceProcess,
    clients: SourceClients,
    modes: SourceModes,
    uptime_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceTerminal {
    cols: u32,
    rows: u32,
    cursor_x: u32,
    cursor_y: u32,
    scrollback_used: u64,
    scrollback_capacity: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceProcess {
    alive: bool,
    exit_code: Option<i32>,
    resources: Option<SourceProcessResources>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceProcessResources {
    rss_kb: u64,
    cpu_percent: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceClients {
    total: u32,
    attached: u32,
    read_only: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceModes {
    sgr_mouse: bool,
    cursor_hidden: bool,
    kitty_keyboard: bool,
    kitty_keyboard_flags: Vec<u32>,
}

fn source_to_wit(source: &CachedSource) -> SessionSource {
    SessionSource {
        id: source.id.clone(),
        observed_at: source.observed_at.clone(),
        lifecycle: match source.lifecycle {
            SourceLifecycle::Running => Lifecycle::Running,
            SourceLifecycle::Exited => Lifecycle::Exited,
            SourceLifecycle::Vanished => Lifecycle::Vanished,
            SourceLifecycle::Absent => Lifecycle::Absent,
        },
        generation: source
            .generation
            .as_ref()
            .map(|generation| match generation {
                SourceGeneration::Number(number) => Generation::Number(*number),
                SourceGeneration::Timestamp(timestamp) => Generation::Timestamp(timestamp.clone()),
            }),
        metadata: source.metadata.as_ref().map(|metadata| Metadata {
            display_name: metadata.display_name.clone(),
            command: metadata.command.clone(),
            cwd: metadata.cwd.clone(),
            created_at: metadata.created_at.clone(),
            exit_code: metadata.exit_code,
            exited_at: metadata.exited_at.clone(),
            tags: metadata.tags.as_ref().map(|tags| {
                tags.iter()
                    .map(|(key, value)| Tag {
                        key: key.clone(),
                        value: value.clone(),
                    })
                    .collect()
            }),
        }),
        runtime: source.runtime.as_ref().map(|runtime| Runtime {
            terminal: Terminal {
                cols: runtime.terminal.cols,
                rows: runtime.terminal.rows,
                cursor_x: runtime.terminal.cursor_x,
                cursor_y: runtime.terminal.cursor_y,
                scrollback_used: runtime.terminal.scrollback_used,
                scrollback_capacity: runtime.terminal.scrollback_capacity,
            },
            process: Process {
                alive: runtime.process.alive,
                exit_code: runtime.process.exit_code,
                resources: runtime
                    .process
                    .resources
                    .as_ref()
                    .map(|resources| ProcessResources {
                        rss_kb: resources.rss_kb,
                        cpu_percent: resources.cpu_percent,
                    }),
            },
            clients: Clients {
                total: runtime.clients.total,
                attached: runtime.clients.attached,
                read_only: runtime.clients.read_only,
            },
            modes: Modes {
                sgr_mouse: runtime.modes.sgr_mouse,
                cursor_hidden: runtime.modes.cursor_hidden,
                kitty_keyboard: runtime.modes.kitty_keyboard,
                kitty_keyboard_flags: runtime.modes.kitty_keyboard_flags.clone(),
            },
            uptime_seconds: runtime.uptime_seconds,
        }),
    }
}

fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 255
        && !matches!(session, "." | "..")
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn contains_not_found(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("not found")
}

fn drain_bounded(
    mut input: impl std::io::Read,
    limit: usize,
) -> Result<(Vec<u8>, bool), PtyStatsError> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0_u8; 8192];
    loop {
        let read = input
            .read(&mut chunk)
            .map_err(|_| PtyStatsError::Unavailable)?;
        if read == 0 {
            return Ok((bytes, false));
        }
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..read.min(remaining)]);
        if read > remaining {
            let mut sink = std::io::sink();
            sink.write_all(&chunk[remaining..read])
                .map_err(|_| PtyStatsError::Unavailable)?;
            std::io::copy(&mut input, &mut sink).map_err(|_| PtyStatsError::Unavailable)?;
            return Ok((bytes, true));
        }
    }
}

fn kill_process_group(process_group: i32) -> bool {
    // SAFETY: a negative pid addresses the dedicated process group created by pre_exec.
    unsafe { libc::kill(-process_group, libc::SIGKILL) == 0 }
}

fn wait_without_reaping(pid: u32) -> std::io::Result<()> {
    let pid =
        i32::try_from(pid).map_err(|_| std::io::Error::other("child pid did not fit in pid_t"))?;
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info points to writable storage and WNOWAIT preserves wait() as the sole reaper.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn resolve_executable(executable: &Path) -> Option<PathBuf> {
    let validation_cwd = std::env::current_dir().ok()?;
    let search_path = std::env::var_os("PATH");
    resolve_executable_at(executable, &validation_cwd, search_path.as_deref())
}

fn resolve_executable_at(
    executable: &Path,
    validation_cwd: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if executable.components().count() > 1 {
        let candidate = if executable.is_absolute() {
            executable.to_path_buf()
        } else {
            validation_cwd.join(executable)
        };
        return executable_is_runnable(&candidate)
            .then(|| candidate.canonicalize().ok())
            .flatten();
    }
    let path = search_path?;
    std::env::split_paths(path).find_map(|directory| {
        let directory = if directory.as_os_str().is_empty() {
            validation_cwd.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            validation_cwd.join(directory)
        };
        let candidate = directory.join(executable);
        executable_is_runnable(&candidate)
            .then(|| candidate.canonicalize().ok())
            .flatten()
    })
}

fn executable_is_runnable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn make_fifo(path: &Path) {
        let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the pathname is a live NUL-terminated byte string owned for the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    }

    fn invocation(config: PtyStatsConfig) -> PtyStatsInvocation {
        let deadline = Instant::now() + config.deadline;
        PtyStatsInvocation {
            config,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            prior_digest: None,
            current_source: None,
            deadline,
            control: Arc::new(ProcessControl::detached()),
        }
    }

    #[test]
    fn config_authorizes_only_executable_cwd_and_deadline() {
        let validation_cwd = std::env::current_dir().unwrap();
        let temporary = tempfile::Builder::new()
            .prefix("st2-pty-resolution-")
            .tempdir_in(&validation_cwd)
            .unwrap();
        let configured_cwd = temporary.path().join("configured");
        std::fs::create_dir_all(temporary.path().join("tools")).unwrap();
        std::fs::create_dir_all(&configured_cwd).unwrap();
        let validated_executable = temporary.path().join("tools/pty");
        write_executable(&validated_executable, "#!/bin/sh\nprintf '[]'\n");
        let relative_executable = validated_executable.strip_prefix(&validation_cwd).unwrap();

        let config =
            PtyStatsConfig::resolve(relative_executable, configured_cwd, Duration::from_secs(1))
                .unwrap();
        assert_eq!(config.executable, validated_executable);
        assert_eq!(
            invocation(config)
                .list("dynamic-session".into())
                .unwrap()
                .current
                .id,
            "dynamic-session"
        );
    }

    #[test]
    fn host_accepts_dynamic_canonical_ids_and_rejects_aliases() {
        assert!(valid_session_id("stable.session-1"));
        for session in ["", ".", "..", "../session", "display name", "slash/name"] {
            assert!(!valid_session_id(session));
        }
    }

    #[test]
    fn list_and_stats_are_sequential_and_cache_typed_source_by_snapshot_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("pty");
        write_executable(
            &executable,
            "#!/bin/sh\nif [ \"$1\" = list ]; then\n  printf '%s' '[{\"name\":\"one\",\"status\":\"running\",\"command\":\"agent\",\"cwd\":\"/workspace\",\"createdAt\":\"created\",\"exitCode\":null,\"exitedAt\":null,\"tags\":{\"owner\":\"agent\"},\"displayName\":\"One\",\"generation\":1}]'\nelse\n  printf '%s' '{\"name\":\"one\",\"status\":\"running\",\"terminal\":{\"cols\":80,\"rows\":24,\"cursorX\":1,\"cursorY\":2,\"scrollbackUsed\":3,\"scrollbackCapacity\":100},\"process\":{\"alive\":true,\"exitCode\":null,\"resources\":{\"rssKb\":10,\"cpuPercent\":1.5}},\"clients\":{\"total\":1,\"attached\":1,\"readOnly\":0},\"modes\":{\"sgrMouse\":false,\"cursorHidden\":false,\"kittyKeyboard\":true,\"kittyKeyboardFlags\":[1]},\"uptimeSeconds\":5,\"createdAt\":\"created\",\"generation\":1,\"exitCode\":null,\"exitedAt\":null,\"tags\":null}'\nfi\n",
        );
        let config =
            PtyStatsConfig::resolve(&executable, temporary.path(), Duration::from_secs(1)).unwrap();
        let cache = Arc::new(Mutex::new(SnapshotCache::default()));
        let mut first = PtyStatsInvocation {
            config: config.clone(),
            cache: Arc::clone(&cache),
            prior_digest: None,
            current_source: None,
            deadline: Instant::now() + config.deadline,
            control: Arc::new(ProcessControl::detached()),
        };
        let listed = first.list("one".into()).unwrap();
        assert!(matches!(&listed.current.lifecycle, Lifecycle::Running));
        assert!(listed.current.runtime.is_none());
        let with_stats = first.stats("one".into()).unwrap();
        assert!(with_stats.runtime.is_some());
        first.bind_snapshot(vec![7; SNAPSHOT_DIGEST_BYTES]).unwrap();

        let deadline = Instant::now() + config.deadline;
        let mut second = PtyStatsInvocation {
            config,
            cache,
            prior_digest: Some([7; SNAPSHOT_DIGEST_BYTES]),
            current_source: None,
            deadline,
            control: Arc::new(ProcessControl::detached()),
        };
        let listed = second.list("one".into()).unwrap();
        assert!(listed.previous.unwrap().runtime.is_some());
    }

    #[test]
    fn list_and_stats_reuse_the_invocation_deadline() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("pty");
        write_executable(
            &executable,
            "#!/bin/sh\nif [ \"$1\" = list ]; then\n  printf '%s' '[{\"name\":\"one\",\"status\":\"running\"}]'\nelse\n  : > \"$PWD/stats-invoked\"\n  exit 64\nfi\n",
        );
        let config =
            PtyStatsConfig::resolve(&executable, temporary.path(), Duration::from_secs(1)).unwrap();
        let control = Arc::new(ProcessControl::detached());
        let mut invocation = PtyStatsInvocation {
            config,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            prior_digest: None,
            current_source: None,
            deadline: Instant::now() + Duration::from_secs(1),
            control: Arc::clone(&control),
        };
        let original_deadline = invocation.deadline;

        invocation.list("one".into()).unwrap();
        assert_eq!(invocation.deadline, original_deadline);
        invocation.deadline = Instant::now();
        assert!(matches!(
            invocation.stats("one".into()),
            Err(PtyStatsError::DeadlineExceeded)
        ));
        assert!(!temporary.path().join("stats-invoked").exists());
        assert!(matches!(*control.child.lock(), ChildOwnership::Pending));
    }

    #[test]
    fn fixed_command_deadline_kills_reaps_and_resets_the_process_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("blocked-pty");
        let fifo = temporary.path().join("block");
        make_fifo(&fifo);
        write_executable(
            &executable,
            "#!/bin/sh\nexec 3< \"$PWD/block\"\nread value <&3\n",
        );
        let config =
            PtyStatsConfig::resolve(&executable, temporary.path(), Duration::from_millis(100))
                .unwrap();
        let control = Arc::new(ProcessControl::detached());
        let deadline = Instant::now() + config.deadline;
        let mut invocation = PtyStatsInvocation {
            config,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            prior_digest: None,
            current_source: None,
            deadline,
            control: Arc::clone(&control),
        };
        assert!(matches!(
            invocation.run(&["list", "--json"]),
            Err(PtyStatsError::DeadlineExceeded)
        ));
        assert!(matches!(*control.child.lock(), ChildOwnership::Pending));
    }
}
