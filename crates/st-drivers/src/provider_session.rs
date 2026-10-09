//! The launch-and-hold-presence body shared by every interactive provider wrapper.
//!
//! An interactive harness cannot be trusted to keep its own control channel alive for the whole
//! session: Claude may close its stdio MCP child after startup, and a pi extension is only as
//! durable as the pi process that loaded it. The wrapper that owns the PTY launch is the one
//! process whose lifetime is exactly the session's, so presence is refreshed from here while that
//! exact child remains alive. Each harness module keeps only what is genuinely harness-specific:
//! how its provider argv is assembled and what environment the harness needs to reach st2 back.

use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{harness_state, status};

/// Refresh cadence for session-owned observed records.
pub(crate) const SESSION_REFRESH: Duration = Duration::from_secs(5 * 60);

pub(crate) const PROVIDER_POLL: Duration = Duration::from_millis(250);
const STOP_GRACE: Duration = Duration::from_secs(5);

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

/// Whether a stop handler recorded a stop. A driver that released its provider to re-execute
/// asks this once the stop signals are blocked, and adopts the provider again to stop it.
pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Set by a driver that is about to re-execute itself into a replaced st binary. Every wrapper
/// loop that sees it returns [`Detached`] with what the next image needs to adopt its provider,
/// and leaves the provider, its terminal, and its observed record exactly as they are.
pub static DETACH: AtomicBool = AtomicBool::new(false);

/// A provider session a wrapper released for adoption by the driver's next image.
///
/// It travels as an error so every wrapper keeps its ordinary `Result<()>` signature: a caller
/// that does not re-execute never sets [`DETACH`] and never sees one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detached {
    pub session: DetachedSession,
}

impl std::fmt::Display for Detached {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the provider session was released for adoption by a replacement driver"
        )
    }
}

impl std::error::Error for Detached {}

/// What each harness hands its next driver image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DetachedSession {
    /// One provider child in the wrapper's terminal process group, observed under a claimed
    /// session incarnation: Claude, pi, and omp.
    Provider { pid: u32, session: String, seq: u64 },
    /// The OpenCode TUI and the loopback server it answers on.
    OpenCode {
        pid: u32,
        session: String,
        seq: u64,
        port: u16,
        password: String,
        version_ok: bool,
        producer_version: Option<String>,
    },
    /// The Codex TUI, its app-server process group, and the write end of the group's watchdog
    /// pipe, which must stay open across the exec or the watchdog ends the group.
    Codex {
        tui_pid: u32,
        server_pid: u32,
        watchdog_pid: u32,
        owner_write_fd: i32,
        socket_path: PathBuf,
        /// Whether the session runs in the provider-safe fallback mode it may have degraded to.
        safe_fallback: bool,
    },
}

impl DetachedSession {
    /// Descriptors the next image must inherit.
    pub fn inherited_descriptors(&self) -> Vec<i32> {
        match self {
            Self::Codex { owner_write_fd, .. } => vec![*owner_write_fd],
            Self::Provider { .. } | Self::OpenCode { .. } => Vec::new(),
        }
    }
}

/// A provider child this wrapper spawned, or one a predecessor image spawned and this image
/// adopted. `execve` keeps the parent relationship, so both can be reaped and signalled.
pub(crate) enum ProviderProcess {
    Spawned(Child),
    Adopted { pid: u32, exit: Option<ExitStatus> },
}

impl ProviderProcess {
    pub(crate) fn adopted(pid: u32) -> Self {
        Self::Adopted { pid, exit: None }
    }

    pub(crate) fn id(&self) -> u32 {
        match self {
            Self::Spawned(child) => child.id(),
            Self::Adopted { pid, .. } => *pid,
        }
    }

    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Spawned(child) => child.try_wait(),
            Self::Adopted { pid, exit } => {
                if exit.is_none() {
                    *exit = reap(*pid, libc::WNOHANG)?;
                }
                Ok(*exit)
            }
        }
    }

    pub(crate) fn wait(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Self::Spawned(child) => child.wait(),
            Self::Adopted { pid, exit } => {
                if let Some(status) = exit {
                    return Ok(*status);
                }
                let status = reap(*pid, 0)?
                    .ok_or_else(|| std::io::Error::other("waitpid returned without a status"))?;
                *exit = Some(status);
                Ok(status)
            }
        }
    }

    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Spawned(child) => child.kill(),
            Self::Adopted { pid, exit } => {
                if exit.is_some() {
                    return Ok(());
                }
                if unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) } == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            }
        }
    }
}

/// Reap `pid` when it has exited. `flags` is `WNOHANG` for a poll or 0 to block.
fn reap(pid: u32, flags: libc::c_int) -> std::io::Result<Option<ExitStatus>> {
    let mut status = 0;
    loop {
        let reaped = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, flags) };
        if reaped == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if reaped == 0 {
            return Ok(None);
        }
        return Ok(Some(ExitStatus::from_raw(status)));
    }
}

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

extern "C" fn on_interrupt_signal(_signal: libc::c_int) {}

pub(crate) fn install_signal_handler() {
    STOP.store(false, Ordering::SeqCst);
    install_stop_handlers();
}

/// Install the stop handlers without clearing a stop that already arrived. A re-executed driver
/// installs them before it unblocks the signals its predecessor blocked across the exec.
pub fn install_stop_handlers() {
    let handler = on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let interrupt = on_interrupt_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        // PTY control uses hangup for a supported restart. Treat it as the same bounded stop as
        // SIGTERM so an interactive provider that ignores or escapes terminal hangup cannot be
        // orphaned behind the wrapper.
        libc::signal(libc::SIGHUP, handler);
        // The terminal also sends SIGINT to this wrapper. Keep the wrapper alive while the
        // provider handles that interactive interrupt itself.
        libc::signal(libc::SIGINT, interrupt);
    }
}

/// How one provider session ended, as the wrapper saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderOutcome {
    /// The child exited on its own, with this status.
    Exited(ExitStatus),
    /// The wrapper stopped its process group; the reaped status when the group yielded inside the
    /// grace window. `None` means the SIGKILL escalation ran — and since that kill targets the
    /// wrapper's own group, code after it usually never runs at all.
    Stopped(Option<ExitStatus>),
    /// [`DETACH`] released this still-running provider for adoption by the next driver image.
    Detached(u32),
}

/// The observed-harness-state handle a wrapper threads through its poll loop. Every operation
/// constructs a fresh writer over the on-disk record, so the wrapper re-stamps or terminates
/// whatever state a hook process wrote in between and never clobbers a fresher observation.
pub(crate) struct SessionObserver {
    agent_dir: PathBuf,
    identity: String,
    harness: &'static str,
    pty_session: String,
    session: String,
    seq: u64,
    /// A terminal-only observer records how the session ended but never re-stamps live state —
    /// for wrappers whose heartbeat belongs to another sibling process (pi's channel).
    heartbeats: bool,
}

impl SessionObserver {
    /// `pty_session` is the wrapper's runtime/task ID — the registry entry whose liveness vouches
    /// for the record. The observer mints the session incarnation token and performs the WRITTEN
    /// ownership claim (superseding whatever a predecessor left, fresh live records included);
    /// the wrapper exports both to its sibling writer processes (hooks, a channel) so ownership
    /// — coalescing, heartbeat eligibility, terminal fencing — is decided by the claim across
    /// all of them. A claim that cannot be written is fatal at construction: acting without
    /// ownership would silently produce a writer every record refuses.
    pub(crate) fn new(
        agent_dir: &Path,
        identity: &str,
        harness: &'static str,
        pty_session: &str,
    ) -> anyhow::Result<Self> {
        Self::with_session(
            agent_dir,
            identity,
            harness,
            pty_session,
            harness_state::session_token(),
        )
    }

    /// Claim an explicitly supplied session incarnation for a host-owned launch attempt.
    pub(crate) fn with_session(
        agent_dir: &Path,
        identity: &str,
        harness: &'static str,
        pty_session: &str,
        session: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(!session.is_empty(), "provider session incarnation is empty");
        let seq = harness_state::claim(agent_dir, identity, harness, &session)?;
        Ok(Self {
            seq,
            agent_dir: agent_dir.to_path_buf(),
            identity: identity.to_string(),
            harness,
            pty_session: pty_session.to_string(),
            session,
            heartbeats: true,
        })
    }

    /// Resume observing a session a predecessor driver image claimed. The ownership claim is
    /// already on disk under this token and sequence, so adopting it writes nothing.
    pub(crate) fn adopt(
        agent_dir: &Path,
        identity: &str,
        harness: &'static str,
        pty_session: &str,
        session: &str,
        seq: u64,
    ) -> Self {
        Self {
            agent_dir: agent_dir.to_path_buf(),
            identity: identity.to_string(),
            harness,
            pty_session: pty_session.to_string(),
            session: session.to_string(),
            seq,
            heartbeats: true,
        }
    }

    /// An observer that records only how the session ended: `heartbeat` is a no-op because a
    /// sibling process owns the live record and its freshness. Adopts that sibling's token so
    /// the terminal record fences exactly this session's records.
    pub(crate) fn terminal_only(
        agent_dir: &Path,
        identity: &str,
        harness: &'static str,
        pty_session: &str,
        session: &str,
        seq: u64,
    ) -> Self {
        Self {
            agent_dir: agent_dir.to_path_buf(),
            identity: identity.to_string(),
            harness,
            pty_session: pty_session.to_string(),
            session: session.to_string(),
            seq,
            heartbeats: false,
        }
    }

    /// The session incarnation token sibling writer processes must adopt.
    pub(crate) fn session(&self) -> &str {
        &self.session
    }

    /// The ownership sequence this session claimed — exported beside the token.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    fn writer(&self) -> harness_state::Writer {
        harness_state::Writer::new(
            &self.agent_dir,
            &self.identity,
            self.harness,
            Some(self.pty_session.clone()),
        )
        .with_ownership(self.session.clone(), self.seq)
    }

    /// Re-stamp whatever live state is on disk. The wrapper's evidence is the provider child it is
    /// polling, so this is called only while that child is alive.
    pub(crate) fn heartbeat(&self) {
        if self.heartbeats {
            let _ = self.writer().heartbeat();
        }
    }

    /// Best-effort terminal record; observation must never turn a clean teardown into an error.
    pub(crate) fn ended(&self, exit: &str) {
        let _ = self.writer().ended(exit);
    }

    /// The terminal record for a session whose provider never ran (or could no longer be
    /// checked): a real ended record, so the claim placeholder is not the last word.
    pub(crate) fn launch_error(&self) {
        let _ = self.writer().observe(
            harness_state::Observation::new(
                harness_state::Activity::Ended,
                harness_state::BlockedOn::None,
                harness_state::InputBuffer::Unknown,
            )
            .with_reason("launch-error")
            .with_exit("exit unknown"),
        );
    }
}

/// The one exit-label map: how every wrapper spells a reaped child's outcome into the observed
/// record's `exit` field.
///
/// `(None, None)` is unreachable for a child this process reaped — `Child::wait`/`try_wait` call
/// `waitpid` with neither `WUNTRACED` nor `WCONTINUED`, so every status they return satisfies
/// `WIFEXITED` or `WIFSIGNALED` — but the label still has to be honest for the arm the type
/// admits, which is why it says "unknown" rather than inventing an ordinary exit.
/// `the_exit_label_map_covers_every_arm` pins the whole table.
pub(crate) fn describe_exit(exit: ExitStatus) -> String {
    match (exit.code(), exit.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "exit unknown".to_string(),
    }
}

/// Run one interactive provider in this wrapper's terminal process group, refreshing presence on
/// `refresh_interval` for exactly as long as the spawned child lives. Fails on a nonzero exit;
/// wrappers that need the exit itself use [`run_provider_observed_with_env_removals`]. With an
/// observer, the terminal record lands on every exit path this process survives.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider(
    provider: &str,
    status_path: Option<&Path>,
    argv: &[String],
    env: &[(String, String)],
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<()> {
    run_provider_with_env_removals(
        provider,
        status_path,
        argv,
        env,
        &[],
        refresh_interval,
        poll,
        stop,
        observed,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider_with_env_removals(
    provider: &str,
    status_path: Option<&Path>,
    argv: &[String],
    env: &[(String, String)],
    removed_env: &[&str],
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<()> {
    let outcome = run_provider_observed_with_env_removals(
        provider,
        status_path,
        argv,
        env,
        removed_env,
        refresh_interval,
        poll,
        stop,
        observed,
    )?;
    finish_provider(provider, outcome, observed)
}

/// Supervise a provider a predecessor driver image spawned, exactly as the wrapper that spawned
/// it would have: the same presence refresh, stop path, and terminal record.
#[allow(clippy::too_many_arguments)]
pub(crate) fn adopt_provider(
    provider: &str,
    status_path: Option<&Path>,
    pid: u32,
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<()> {
    let outcome = supervise_provider(
        provider,
        status_path,
        ProviderProcess::adopted(pid),
        refresh_interval,
        poll,
        stop,
        &DETACH,
        observed,
    )?;
    finish_provider(provider, outcome, observed)
}

fn finish_provider(
    provider: &str,
    outcome: ProviderOutcome,
    observed: Option<&SessionObserver>,
) -> Result<()> {
    match outcome {
        ProviderOutcome::Exited(exit) => {
            if let Some(observed) = observed {
                observed.ended(&describe_exit(exit));
            }
            completed_provider(provider, exit)
        }
        ProviderOutcome::Stopped(_) => Ok(()),
        ProviderOutcome::Detached(pid) => Err(detached_provider(pid, observed).into()),
    }
}

/// The adoption record for one provider child and the session incarnation observing it.
pub(crate) fn detached_provider(pid: u32, observed: Option<&SessionObserver>) -> Detached {
    Detached {
        session: DetachedSession::Provider {
            pid,
            session: observed
                .map(|observed| observed.session().to_owned())
                .unwrap_or_default(),
            seq: observed.map(SessionObserver::seq).unwrap_or_default(),
        },
    }
}

/// [`run_provider`], but reporting how the session ended instead of judging it, so a wrapper can
/// record its own terminal observation before deciding what the exit means. The stop path still
/// writes the observer's terminal record in-line, because after SIGKILL escalation no caller code
/// is guaranteed to run.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider_observed(
    provider: &str,
    status_path: Option<&Path>,
    argv: &[String],
    env: &[(String, String)],
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<ProviderOutcome> {
    run_provider_observed_with_env_removals(
        provider,
        status_path,
        argv,
        env,
        &[],
        refresh_interval,
        poll,
        stop,
        observed,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider_observed_with_env_removals(
    provider: &str,
    status_path: Option<&Path>,
    argv: &[String],
    env: &[(String, String)],
    removed_env: &[&str],
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<ProviderOutcome> {
    let (program, args) = argv
        .split_first()
        .with_context(|| format!("{provider} provider argv is empty"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    apply_provider_environment(&mut command, env, removed_env);
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    // The error arms are terminal outcomes too: the claim placeholder must not stand as the
    // visible state after a launch that never ran — while the ordinary nonzero-exit path keeps
    // its real exit and is deliberately not covered here.
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(observed) = observed {
                observed.launch_error();
            }
            return Err(error).with_context(|| format!("starting {provider} provider {program}"));
        }
    };
    supervise_provider(
        provider,
        status_path,
        ProviderProcess::Spawned(child),
        refresh_interval,
        poll,
        stop,
        &DETACH,
        observed,
    )
}

/// [`adopt_provider`], reporting how the session ended instead of judging it.
pub(crate) fn adopt_provider_observed(
    provider: &str,
    status_path: Option<&Path>,
    pid: u32,
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<ProviderOutcome> {
    supervise_provider(
        provider,
        status_path,
        ProviderProcess::adopted(pid),
        refresh_interval,
        poll,
        stop,
        &DETACH,
        observed,
    )
}

#[allow(clippy::too_many_arguments)]
fn supervise_provider(
    provider: &str,
    status_path: Option<&Path>,
    mut child: ProviderProcess,
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    detach: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<ProviderOutcome> {
    let mut next_refresh = Instant::now();
    let mut next_observation = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return stop_provider_group(&mut child, observed).map(ProviderOutcome::Stopped);
        }
        if detach.load(Ordering::SeqCst) {
            // A provider that exited at the same moment is reported as the exit it was.
            if let Some(exit) = child.try_wait().ok().flatten() {
                return Ok(ProviderOutcome::Exited(exit));
            }
            return Ok(ProviderOutcome::Detached(child.id()));
        }
        match child.try_wait() {
            Ok(Some(exit)) => return Ok(ProviderOutcome::Exited(exit)),
            Ok(None) => {}
            Err(error) => {
                if let Some(observed) = observed {
                    observed.launch_error();
                }
                return Err(error).with_context(|| format!("checking {provider} provider"));
            }
        }
        let now = Instant::now();
        if now >= next_refresh {
            if let Some(status_path) = status_path {
                let _ = status::refresh(status_path);
            }
            next_refresh = now + refresh_interval;
        }
        if now >= next_observation {
            if let Some(observed) = observed {
                observed.heartbeat();
            }
            next_observation = now + harness_state::HARNESS_STATE_REFRESH;
        }
        thread::sleep(poll.min(next_refresh.saturating_duration_since(Instant::now())));
    }
}

fn apply_provider_environment(
    command: &mut Command,
    env: &[(String, String)],
    removed_env: &[&str],
) {
    // A fresh provider owns a new wrapper environment, even when launched below an old seat.
    // Never hand it another harness's legacy ownership or mandatory-resume fields.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| {
            key.starts_with("ST2_CLAUDE_")
                || key.starts_with("ST2_PI_CHANNEL_")
                || key.starts_with("ST2_OMP_CHANNEL_")
        }) {
            command.env_remove(key);
        }
    }
    for key in removed_env {
        command.env_remove(key);
        if let Some(legacy) = crate::contracts::legacy_env_name(key) {
            command.env_remove(legacy);
        }
    }
    for (key, value) in env {
        if let Some(legacy) = crate::contracts::legacy_env_name(key) {
            command.env_remove(legacy);
        }
        command.env(key, value);
    }
}

pub(crate) fn completed_provider(provider: &str, exit: ExitStatus) -> Result<()> {
    anyhow::ensure!(exit.success(), "{provider} provider exited with {exit}");
    Ok(())
}

pub(crate) fn stop_provider_group(
    child: &mut ProviderProcess,
    observed: Option<&SessionObserver>,
) -> Result<Option<ExitStatus>> {
    let process_group = unsafe { libc::getpgrp() };
    anyhow::ensure!(
        process_group > 1,
        "refusing to signal process group {process_group}"
    );
    unsafe {
        // Address the owned child as well as the terminal process group. Interactive providers
        // may create a new process group while enabling remote control; the Child handle keeps
        // this PID reserved until it is reaped, so this cannot hit a reused process.
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        libc::kill(-process_group, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if let Some(exit) = child.try_wait()? {
            if let Some(observed) = observed {
                observed.ended(&describe_exit(exit));
            }
            return Ok(Some(exit));
        }
        thread::sleep(Duration::from_millis(25));
    }
    // The escalation SIGKILLs this wrapper's own process group, so the wrapper dies with the
    // provider and nothing after the kill is guaranteed to run. The terminal record must land
    // first: a liveness record that stops being written is still being read.
    if let Some(observed) = observed {
        observed.ended("signal 9");
    }
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
        libc::kill(-process_group, libc::SIGKILL);
    }
    Ok(child.wait().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_supervision_keeps_observations_without_creating_status() {
        let root = tempfile::tempdir().unwrap();
        let observer = SessionObserver::new(root.path(), "h.native", "claude", "native").unwrap();
        let stop = AtomicBool::new(false);
        run_provider(
            "native",
            None,
            &["sh".into(), "-c".into(), "sleep 0.05".into()],
            &[],
            Duration::from_millis(10),
            Duration::from_millis(5),
            &stop,
            Some(&observer),
        )
        .unwrap();
        assert!(!root.path().join("status").exists());
        let record =
            harness_state::read(&harness_state::harness_state_path(root.path()), None).unwrap();
        assert_eq!(record.state, harness_state::Activity::Ended);
        assert_eq!(record.exit.as_deref(), Some("exit 0"));
    }

    /// n6: an unspawnable provider is a terminal outcome — the claim placeholder must not stand.
    #[test]
    fn an_unspawnable_provider_writes_a_real_terminal_record() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let observer = SessionObserver::new(
            tmp.path(),
            "example-linux.worker",
            "claude",
            "example-linux.worker",
        )
        .unwrap();
        let stop = AtomicBool::new(false);
        let result = run_provider_observed(
            "test",
            Some(&crate::status::status_path(tmp.path())),
            &["/nonexistent/provider-binary".to_string()],
            &[],
            Duration::from_secs(60),
            Duration::from_millis(5),
            &stop,
            Some(&observer),
        );
        assert!(result.is_err());
        let record =
            harness_state::read(&harness_state::harness_state_path(tmp.path()), None).unwrap();
        assert_eq!(record.state, Activity::Ended);
        assert_eq!(record.exit.as_deref(), Some("exit unknown"));
        assert_eq!(record.reason.as_deref(), Some("launch-error"));
    }

    /// The whole `(status, signal) -> label` table, including the `(None, None)` arm no reaped
    /// child can produce. Every wrapper's terminal record now spells its exit through this map,
    /// so the table is the contract: a fifth harness that wants a different word has to change
    /// it here, in front of this test, rather than forking a private copy that quietly disagrees.
    #[test]
    fn the_exit_label_map_covers_every_arm() {
        // `wait_status` values as `waitpid` yields them: `code << 8` for an ordinary exit,
        // the bare signal number for a killed child, and `0x7f` for the stopped shape only
        // `WUNTRACED` could deliver — which is what makes `(None, None)` constructible at all.
        for (raw, label) in [
            (0, "exit 0"),
            (3 << 8, "exit 3"),
            (127 << 8, "exit 127"),
            (libc::SIGKILL, "signal 9"),
            (libc::SIGTERM, "signal 15"),
            (0x7f, "exit unknown"),
        ] {
            let exit = ExitStatus::from_raw(raw);
            assert_eq!(
                describe_exit(exit),
                label,
                "raw wait status {raw:#x} => {:?}/{:?}",
                exit.code(),
                exit.signal()
            );
        }
        // The arm the label calls unknown really is the one neither half of the pair answers.
        let stopped = ExitStatus::from_raw(0x7f);
        assert_eq!((stopped.code(), stopped.signal()), (None, None));
    }
    fn spawn_sleeper(seconds: &str) -> u32 {
        // Dropping the handle neither kills nor reaps the child, exactly like a predecessor image
        // that re-executed: the PID stays this process's child.
        Command::new("sleep").arg(seconds).spawn().unwrap().id()
    }

    #[test]
    fn an_adopted_provider_is_supervised_to_its_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = spawn_sleeper("0.2");
        let outcome = supervise_provider(
            "test",
            Some(&crate::status::status_path(tmp.path())),
            ProviderProcess::adopted(pid),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        match outcome {
            ProviderOutcome::Exited(exit) => assert!(exit.success(), "{exit:?}"),
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    #[test]
    fn detaching_releases_a_live_provider_with_its_observed_session() {
        let tmp = tempfile::tempdir().unwrap();
        let observer = SessionObserver::new(
            tmp.path(),
            "example-linux.worker",
            "claude",
            "example-linux.worker",
        )
        .unwrap();
        let pid = spawn_sleeper("30");
        let outcome = supervise_provider(
            "test",
            Some(&crate::status::status_path(tmp.path())),
            ProviderProcess::adopted(pid),
            Duration::from_secs(60),
            Duration::from_millis(5),
            &AtomicBool::new(false),
            &AtomicBool::new(true),
            Some(&observer),
        )
        .unwrap();
        assert_eq!(outcome, ProviderOutcome::Detached(pid));
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            0,
            "the provider must survive"
        );
        let error = anyhow::Error::from(detached_provider(pid, Some(&observer)))
            .context("running interactive Claude driver");
        let detached = error
            .downcast_ref::<Detached>()
            .expect("a detached session");
        assert_eq!(
            detached.session,
            DetachedSession::Provider {
                pid,
                session: observer.session().to_owned(),
                seq: observer.seq(),
            }
        );
        // The next image adopts the same child and sees its real end.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let adopted = SessionObserver::adopt(
            tmp.path(),
            "example-linux.worker",
            "claude",
            "example-linux.worker",
            observer.session(),
            observer.seq(),
        );
        let outcome = supervise_provider(
            "test",
            Some(&crate::status::status_path(tmp.path())),
            ProviderProcess::adopted(pid),
            Duration::from_secs(60),
            Duration::from_millis(5),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            Some(&adopted),
        )
        .unwrap();
        assert_eq!(
            outcome,
            ProviderOutcome::Exited(ExitStatus::from_raw(libc::SIGKILL))
        );
    }

    #[test]
    fn a_detached_session_round_trips_through_resume_state() {
        for session in [
            DetachedSession::Provider {
                pid: 7,
                session: "token".into(),
                seq: 3,
            },
            DetachedSession::OpenCode {
                pid: 7,
                session: "token".into(),
                seq: 3,
                port: 4096,
                password: "secret".into(),
                version_ok: true,
                producer_version: Some("1.18.19".into()),
            },
            DetachedSession::Codex {
                tui_pid: 7,
                server_pid: 8,
                watchdog_pid: 9,
                owner_write_fd: 11,
                socket_path: "/tmp/app-server.sock".into(),
                safe_fallback: false,
            },
        ] {
            let json = serde_json::to_string(&session).unwrap();
            let back: DetachedSession = serde_json::from_str(&json).unwrap();
            assert_eq!(back, session);
        }
        assert_eq!(
            DetachedSession::Codex {
                tui_pid: 7,
                server_pid: 8,
                watchdog_pid: 9,
                owner_write_fd: 11,
                socket_path: "/tmp/app-server.sock".into(),
                safe_fallback: false,
            }
            .inherited_descriptors(),
            [11]
        );
    }

    #[test]
    fn an_explicit_session_incarnation_is_claimed_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let _observer = SessionObserver::with_session(
            tmp.path(),
            "example-linux.worker",
            "claude",
            "example-linux.worker",
            "attempt-exact".into(),
        )
        .unwrap();
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(harness_state::harness_state_path(tmp.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(record["incarnation"], "attempt-exact");

        let error = SessionObserver::with_session(
            tmp.path(),
            "example-linux.worker",
            "claude",
            "example-linux.worker",
            String::new(),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("incarnation is empty"));
    }

    #[test]
    fn explicit_environment_removal_precedes_managed_values() {
        const FENCE: &str = "ST2_TEST_PROVIDER_FENCE";
        let value_for = |command: &Command| {
            command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(FENCE))
                .map(|(_, value)| value.map(|value| value.to_owned()))
        };

        let mut ordinary = Command::new("true");
        ordinary.env(FENCE, "ambient");
        apply_provider_environment(&mut ordinary, &[], &[FENCE]);
        assert_eq!(value_for(&ordinary), Some(None));

        let mut required = Command::new("true");
        required.env(FENCE, "ambient");
        apply_provider_environment(
            &mut required,
            &[(FENCE.to_string(), "required".to_string())],
            &[FENCE],
        );
        assert_eq!(
            value_for(&required),
            Some(Some(std::ffi::OsString::from("required")))
        );
    }
    #[test]
    fn fresh_provider_exports_current_names_and_removes_both_resume_fences() {
        let mut command = Command::new("true");
        apply_provider_environment(
            &mut command,
            &[("ST_CLAUDE_SESSION".into(), "fresh".into())],
            &[
                "ST_CLAUDE_EXPECTED_NATIVE_SESSION",
                "ST_CLAUDE_RESUME_GENERATION",
            ],
        );
        let vars = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(vars["ST_CLAUDE_SESSION"].as_deref(), Some("fresh"));
        for key in [
            "ST2_CLAUDE_SESSION",
            "ST_CLAUDE_EXPECTED_NATIVE_SESSION",
            "ST2_CLAUDE_EXPECTED_NATIVE_SESSION",
            "ST_CLAUDE_RESUME_GENERATION",
            "ST2_CLAUDE_RESUME_GENERATION",
        ] {
            assert_eq!(vars[key], None, "{key} must not reach a fresh provider");
        }
    }
}
