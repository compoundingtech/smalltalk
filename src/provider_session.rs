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

use crate::{harness_state, status};

pub(crate) const PROVIDER_POLL: Duration = Duration::from_millis(250);
const STOP_GRACE: Duration = Duration::from_secs(5);

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

extern "C" fn on_interrupt_signal(_signal: libc::c_int) {}

pub(crate) fn install_signal_handler() {
    STOP.store(false, Ordering::SeqCst);
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
/// wrappers that need the exit itself use [`run_provider_observed`]. With an observer, the
/// terminal record lands on every exit path this process survives.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider(
    provider: &str,
    status_path: &Path,
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
    status_path: &Path,
    argv: &[String],
    env: &[(String, String)],
    removed_env: &[&str],
    refresh_interval: Duration,
    poll: Duration,
    stop: &AtomicBool,
    observed: Option<&SessionObserver>,
) -> Result<()> {
    match run_provider_observed_with_env_removals(
        provider,
        status_path,
        argv,
        env,
        removed_env,
        refresh_interval,
        poll,
        stop,
        observed,
    )? {
        ProviderOutcome::Exited(exit) => {
            if let Some(observed) = observed {
                observed.ended(&describe_exit(exit));
            }
            completed_provider(provider, exit)
        }
        ProviderOutcome::Stopped(_) => Ok(()),
    }
}

/// [`run_provider`], but reporting how the session ended instead of judging it, so a wrapper can
/// record its own terminal observation before deciding what the exit means. The stop path still
/// writes the observer's terminal record in-line, because after SIGKILL escalation no caller code
/// is guaranteed to run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_provider_observed(
    provider: &str,
    status_path: &Path,
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
    status_path: &Path,
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
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(observed) = observed {
                observed.launch_error();
            }
            return Err(error).with_context(|| format!("starting {provider} provider {program}"));
        }
    };
    let mut next_refresh = Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            return stop_provider_group(&mut child, observed).map(ProviderOutcome::Stopped);
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
            let _ = status::refresh(status_path);
            if let Some(observed) = observed {
                observed.heartbeat();
            }
            next_refresh = now + refresh_interval;
        }
        thread::sleep(poll.min(next_refresh.saturating_duration_since(Instant::now())));
    }
}

fn apply_provider_environment(
    command: &mut Command,
    env: &[(String, String)],
    removed_env: &[&str],
) {
    for key in removed_env {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
}

pub(crate) fn completed_provider(provider: &str, exit: ExitStatus) -> Result<()> {
    anyhow::ensure!(exit.success(), "{provider} provider exited with {exit}");
    Ok(())
}

pub(crate) fn stop_provider_group(
    child: &mut Child,
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

    /// n6: an unspawnable provider is a terminal outcome — the claim placeholder must not stand.
    #[test]
    fn an_unspawnable_provider_writes_a_real_terminal_record() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let observer =
            SessionObserver::new(tmp.path(), "example-linux.worker", "claude", "example-linux.worker").unwrap();
        let stop = AtomicBool::new(false);
        let result = run_provider_observed(
            "test",
            &crate::status::status_path(tmp.path()),
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
}
