use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result};
use notify::Watcher as _;
use serde_json::Value;
use sha2::Digest as _;
use tokio::sync::{Notify, watch};

use crate::checkout::Checkout;
use crate::mission::{
    CANDIDATE_INDEX_INPUT, LOOP_FEEDBACK_INPUT, LOOP_ITEM_INPUT, LOOP_ROUND_INPUT,
};
use crate::model::{
    AttentionRequest, ClaimInput, CurrentHarnessView, DependencySpec, DesiredSubject, GateContext,
    GateSpec, LaunchSpec, LoopCandidateSelector, LoopExhaustionSpec, LoopSpec, MemberKind,
    MemberLifecycle, MemberSpec, MessageView, MetricSource, MissionInputKind, MissionRunRequest,
    MissionRunView, MissionSpec, MissionState, RestartIntensity, RestartType, StepRunView,
    StepSpec, SubscriptionSpec, UsedMissionSpec, WorkSelector,
};
use crate::resource::{
    ObservationRequest, ProviderRateLimit, ProviderUnauthenticated, RegisteredResourceProvider,
    ResourceProvider,
};
use crate::store::Store;

const HARNESS_READINESS_DEADLINE_MS: u128 = 60_000;
const WORK_WAKE_RETRY_MS: u128 = 15_000;
// A mechanical gate may run for minutes. Each poll reruns the entire host reconciliation
// pass, including PTY snapshots. Ten seconds bounds result recognition without keeping a
// busy host in near-continuous reconciliation while gates are still running.
const GATE_POLL_INTERVAL: Duration = Duration::from_secs(10);
const WORK_WAKE_MAX_ATTEMPTS: u32 = 3;
// A new harness can spend longer than the retry sequence reading its boot
// contract before it claims work. Keep the quick delivery retries, but do not
// diagnose a failed wake while that bounded startup window is still open.
const WORK_WAKE_EXHAUST_GRACE_MS: u128 = 5 * 60_000;
const CODEX_CRASH_LOOP_ATTEMPTS: usize = 3;
const CODEX_CRASH_LOOP_INTERVAL_MS: u128 = 5 * 60_000;
// Claude shows its workspace trust prompt within a few seconds of starting. Rechecking the screen
// on this grid until readiness finds it well before the readiness deadline.
const CLAUDE_TRUST_SCREEN_RECHECK_MS: u128 = 2_000;
const CLAUDE_TRUST_RECOVERY_ATTEMPTS: usize = 3;
const CLAUDE_TRUST_RECOVERY_WINDOW_MS: u128 = 10 * 60_000;
// A failed checkout fetch or worktree command waits this long before Git runs again.
const CHECKOUT_RETRY_MS: u128 = 30_000;
// A deadline source that could not be read is read again this soon, so the deadlines it holds
// are late by at most this much.
const DEADLINE_SOURCE_RETRY_MS: u128 = 5_000;
// Run cleanup ends this long after it began even if an owned runtime never reports stopped.
const CLEANUP_DEADLINE: Duration = Duration::from_secs(15 * 60);
const DECLARED_CHECKOUT_LIMIT: usize = 4096;

#[cfg(test)]
thread_local! {
    static DECLARATION_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The screen line on which Claude asks for /login. Claude prints the prompt as its own line,
/// at most after a status glyph, so a line that only quotes the phrase, such as source code or
/// grep output a session prints, does not match.
fn claude_login_expired(screen: &str) -> Option<&str> {
    screen.lines().map(str::trim).find(|line| {
        let text = line.trim_start_matches(['●', '⎿']).trim_start();
        text.starts_with("Login expired · Please run /login")
            || text.starts_with("Not logged in · Run /login")
    })
}

/// Claude's workspace trust dialog as `pty peek --plain` renders it. Every phrase must be present,
/// so a transcript that quotes one of them is not mistaken for the dialog. Whitespace is collapsed
/// because the question wraps with the terminal width.
fn claude_trust_prompt(screen: &str) -> bool {
    let screen = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    [
        "Accessing workspace:",
        "Quick safety check: Is this a project you created or one you trust?",
        "Yes, I trust this folder",
    ]
    .iter()
    .all(|phrase| screen.contains(phrase))
}

fn claim_incarnation(claim: &crate::model::ClaimRecord) -> Option<&str> {
    claim
        .body
        .pointer("/fields/incarnation_id")
        .and_then(Value::as_str)
}

fn provider_capacity_retry_key(claim_id: &str) -> String {
    format!("provider-capacity-retry:{claim_id}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeObservation {
    pub runtime_id: String,
    pub terminal: bool,
    pub status: String,
    pub exit_code: Option<i64>,
    pub incarnation_id: Option<String>,
}

pub trait RuntimeControl: Send + Sync + 'static {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>>;
    fn observe_exec(&self, runtime_id: &str) -> Result<Option<RuntimeObservation>>;
    fn start(&self, member: &MemberSpec) -> Result<()>;
    fn stop(
        &self,
        runtime_id: &str,
        terminal: bool,
        expected_incarnation: Option<&str>,
    ) -> Result<()>;
    fn kill(
        &self,
        runtime_id: &str,
        terminal: bool,
        expected_incarnation: Option<&str>,
    ) -> Result<()>;
    fn remove(&self, runtime_id: &str, terminal: bool) -> Result<()>;
    fn attach(&self, runtime_id: &str) -> Result<()>;
    fn screen(&self, runtime_id: &str) -> Result<String>;
    fn send_key(&self, runtime_id: &str, key: &str) -> Result<()>;
    fn read_exec_log(&self, runtime_id: &str) -> Result<Option<String>>;
}

pub struct NativeRuntime {
    pty: st_runtime::PtyRuntime,
    exec: st_runtime::ExecRuntime,
    /// Goes first on every member's PATH so its `git` and `gh` calls are recorded.
    recorder: Option<PathBuf>,
}

impl NativeRuntime {
    pub fn new(state_dir: &Path, pty_root: Option<&Path>, pty_binary: &Path) -> Self {
        Self {
            pty: st_runtime::PtyRuntime::new(
                pty_root
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| state_dir.join("pty")),
            )
            .with_binary(pty_binary.to_string_lossy()),
            exec: st_runtime::ExecRuntime::new(state_dir.join("exec"), state_dir.join("logs")),
            recorder: None,
        }
    }
    pub fn with_recorder(mut self, directory: Option<PathBuf>) -> Self {
        self.recorder = directory;
        self
    }
    fn pty(&self) -> Result<st_runtime::PtyRuntime> {
        Ok(self
            .pty
            .clone()
            .with_environment(crate::environment::snapshot()?))
    }
}

/// Puts the recorder directory first on a member's PATH, after the declaration and the st3
/// executable directory are applied, so no authored PATH can place a program before it.
fn record_member_commands(
    environment: &mut BTreeMap<String, String>,
    recorder: Option<&Path>,
) -> Result<()> {
    let Some(recorder) = recorder else {
        return Ok(());
    };
    let path =
        crate::recorder::prepend(recorder, environment.get("PATH").map(std::ffi::OsStr::new))?;
    environment.insert("PATH".into(), path.to_string_lossy().into_owned());
    Ok(())
}

fn observed_pty_status(observation: &st_runtime::PtyObservation) -> String {
    if observation.status == "running"
        && observation
            .pid
            .is_some_and(|pid| !local_process_is_alive(pid))
    {
        return "vanished".into();
    }
    observation.status.clone()
}

#[cfg(unix)]
fn local_process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn local_process_is_alive(_pid: u32) -> bool {
    true
}

impl RuntimeControl for NativeRuntime {
    fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
        self.pty()?
            .snapshot()?
            .into_iter()
            .map(|item| {
                let incarnation_id = match (&item.pid, &item.created_at) {
                    (Some(pid), Some(created)) => Some(format!("{pid}:{created}")),
                    _ => None,
                };
                let status = observed_pty_status(&item);
                Ok(RuntimeObservation {
                    runtime_id: item.name,
                    terminal: true,
                    status,
                    exit_code: item.exit_code,
                    incarnation_id,
                })
            })
            .collect()
    }

    fn observe_exec(&self, runtime_id: &str) -> Result<Option<RuntimeObservation>> {
        Ok(self
            .exec
            .observe(runtime_id)?
            .map(|observation| match observation {
                st_runtime::ExecObservation::Running(generation) => RuntimeObservation {
                    runtime_id: runtime_id.into(),
                    terminal: false,
                    status: "running".into(),
                    exit_code: None,
                    incarnation_id: Some(generation.generation_id),
                },
                st_runtime::ExecObservation::Exited(generation) => RuntimeObservation {
                    runtime_id: runtime_id.into(),
                    terminal: false,
                    status: "exited".into(),
                    exit_code: generation.exit_code.map(i64::from),
                    incarnation_id: Some(generation.generation_id),
                },
                st_runtime::ExecObservation::Indeterminate(reason) => RuntimeObservation {
                    runtime_id: runtime_id.into(),
                    terminal: false,
                    status: "indeterminate".into(),
                    exit_code: None,
                    incarnation_id: Some(reason),
                },
            }))
    }

    fn start(&self, member: &MemberSpec) -> Result<()> {
        let executable = launch_executable()?;
        let mut environment = st_runtime::overlay_environment(
            crate::environment::snapshot()?,
            &member.environment,
            &executable,
        )?;
        record_member_commands(&mut environment, self.recorder.as_deref())?;
        let mut launch = st_runtime::Launch::from(&member.launch);
        match &mut launch {
            st_runtime::Launch::Shell(source) => {
                st_runtime::expand_path_placeholder(source, &environment);
            }
            st_runtime::Launch::Argv(argv) => {
                for value in argv {
                    st_runtime::expand_path_placeholder(value, &environment);
                }
            }
        }
        // Resolve before crossing the PTY/isolation boundary; service-manager PATH is
        // unrelated to the environment captured from the account's login shell.
        match &mut launch {
            st_runtime::Launch::Shell(source) => {
                launch = st_runtime::Launch::Argv(vec![
                    st_runtime::resolve_executable("sh", &environment)?
                        .to_string_lossy()
                        .into_owned(),
                    "-c".into(),
                    source.clone(),
                ]);
            }
            st_runtime::Launch::Argv(argv) => {
                let program = argv
                    .first_mut()
                    .context("an argv launch must contain a program")?;
                *program = st_runtime::resolve_executable(program, &environment)?
                    .to_string_lossy()
                    .into_owned();
            }
        }
        let cwd = PathBuf::from(&member.cwd);
        if member.terminal {
            let pty_binary = st_runtime::resolve_executable("pty", &environment)?;
            self.pty()?
                .clone()
                .with_binary(pty_binary.to_string_lossy())
                .spawn(
                    &member.runtime_id,
                    &launch,
                    &cwd,
                    &environment,
                    member.display_name.as_deref(),
                    &member.tags,
                )
        } else {
            self.exec
                .spawn(&member.runtime_id, &launch, &cwd, &environment)
                .map(|_| ())
        }
    }

    fn stop(
        &self,
        runtime_id: &str,
        terminal: bool,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        if terminal {
            self.pty()?.stop_if(runtime_id, expected_incarnation)
        } else {
            self.exec.stop_if(runtime_id, expected_incarnation)
        }
    }

    fn kill(
        &self,
        runtime_id: &str,
        terminal: bool,
        expected_incarnation: Option<&str>,
    ) -> Result<()> {
        if terminal {
            self.pty()?.kill_if(runtime_id, expected_incarnation)
        } else {
            self.exec.kill_if(runtime_id, expected_incarnation)
        }
    }

    fn remove(&self, runtime_id: &str, terminal: bool) -> Result<()> {
        if terminal {
            if self
                .pty()?
                .snapshot()?
                .iter()
                .any(|item| item.name == runtime_id)
            {
                self.pty()?.remove(runtime_id)
            } else {
                Ok(())
            }
        } else {
            self.exec.remove(runtime_id)
        }
    }

    fn attach(&self, runtime_id: &str) -> Result<()> {
        self.pty()?.attach(runtime_id)
    }

    fn screen(&self, runtime_id: &str) -> Result<String> {
        self.pty()?.screen(runtime_id)
    }

    fn send_key(&self, runtime_id: &str, key: &str) -> Result<()> {
        self.pty()?.send_key(runtime_id, key)
    }

    fn read_exec_log(&self, runtime_id: &str) -> Result<Option<String>> {
        self.exec.read_log(runtime_id)
    }
}

/// A test hook that fails one item of a reconcile pass where the reconciler takes it up.
///
/// `fault(scope, subject)` returns the error that item should fail with, or panics to fail it
/// with a panic. The daemon never installs one; fault-isolation tests use it to fail each kind of
/// item on its own.
pub trait FaultInjection: Send + Sync + 'static {
    fn fault(&self, scope: &str, subject: &str) -> Option<String>;
}

pub struct Reconciler<R = NativeRuntime> {
    store: Arc<Store>,
    runtime: Arc<R>,
    host: String,
    endpoint: String,
    driver_state_dir: PathBuf,
    runtime_environment: BTreeMap<String, String>,
    notify: Arc<Notify>,
    event_notify: watch::Sender<u64>,
    armed_schedules: Arc<Mutex<std::collections::HashSet<String>>>,
    armed_observers: Arc<Mutex<std::collections::HashSet<String>>>,
    gate_poll_armed: Arc<AtomicBool>,
    observer_deadlines: Arc<Mutex<HashMap<String, u128>>>,
    observer_cursors: Arc<Mutex<HashMap<String, Option<String>>>>,
    delayed_restarts: Arc<Mutex<HashMap<String, u128>>>,
    /// When a failed `checkout` may run Git again, and why it failed, by agent subject.
    checkout_retries: Arc<Mutex<HashMap<String, (u128, String)>>>,
    /// The last agent declaration's run-end checkout and workspace, by subject, with the store
    /// index of the subject's newest declaration it was read from.
    declared_checkouts: Mutex<HashMap<String, (u64, Option<(Checkout, String)>)>>,
    materialized_mission_generations: Mutex<BTreeSet<String>>,
    retired_predecessor_generations: Mutex<BTreeSet<String>>,
    #[cfg(test)]
    mission_declaration_parses: std::sync::atomic::AtomicUsize,
    file_watchers: Arc<Mutex<HashMap<String, notify::RecommendedWatcher>>>,
    file_watchers_used: Arc<Mutex<HashSet<String>>>,
    file_observations: Arc<Mutex<HashMap<String, FileStamp>>>,
    resource_provider: Arc<dyn ResourceProvider>,
    /// Open faults by subject and scope, loaded from the graph on first use.
    faults: Mutex<Option<BTreeMap<(String, String), String>>>,
    /// Faults that could not be recorded in the graph during the current pass.
    unrecorded_faults: Mutex<Vec<String>>,
    fault_injection: Option<Arc<dyn FaultInjection>>,
    /// How long run cleanup waits for its runtimes to stop before the run ends without them.
    cleanup_deadline: Duration,
    /// Unit tests fail a pass that raises a fault unless they opt in, so an isolated error
    /// cannot hide inside a test that expects a clean pass.
    #[cfg(test)]
    raised_faults: Mutex<Option<Vec<String>>>,
}

#[derive(Clone, Eq, PartialEq)]
struct FileStamp {
    modified: Option<std::time::SystemTime>,
    size: u64,
    mode: u32,
}

impl FileStamp {
    fn read(path: &Path) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            size: metadata.len(),
            mode: metadata.permissions().mode() & 0o7777,
        })
    }
}

impl Reconciler<NativeRuntime> {
    #[allow(clippy::too_many_arguments)]
    pub fn native(
        store: Arc<Store>,
        state_dir: &Path,
        pty_root: Option<&Path>,
        pty_binary: &Path,
        host: String,
        endpoint: String,
        notify: Arc<Notify>,
        event_notify: watch::Sender<u64>,
        recorder: Option<PathBuf>,
    ) -> Result<Self> {
        let selected_pty_root = pty_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state_dir.join("pty"));
        Ok(Self {
            store,
            runtime: Arc::new(
                NativeRuntime::new(state_dir, Some(&selected_pty_root), pty_binary)
                    .with_recorder(recorder),
            ),
            host,
            endpoint,
            driver_state_dir: state_dir.join("drivers"),
            runtime_environment: BTreeMap::from([
                (
                    "PTY_ROOT".into(),
                    selected_pty_root.to_string_lossy().into_owned(),
                ),
                (
                    "ST_HOOKS".into(),
                    st2::hooks::versioned_hooks_dir()?
                        .to_string_lossy()
                        .into_owned(),
                ),
            ]),
            notify,
            event_notify,
            armed_schedules: Arc::new(Mutex::new(std::collections::HashSet::new())),
            armed_observers: Arc::new(Mutex::new(std::collections::HashSet::new())),
            gate_poll_armed: Arc::new(AtomicBool::new(false)),
            observer_deadlines: Arc::new(Mutex::new(HashMap::new())),
            observer_cursors: Arc::new(Mutex::new(HashMap::new())),
            delayed_restarts: Arc::new(Mutex::new(HashMap::new())),
            checkout_retries: Arc::new(Mutex::new(HashMap::new())),
            declared_checkouts: Mutex::new(HashMap::new()),
            materialized_mission_generations: Mutex::new(BTreeSet::new()),
            retired_predecessor_generations: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            mission_declaration_parses: std::sync::atomic::AtomicUsize::new(0),
            file_watchers: Arc::new(Mutex::new(HashMap::new())),
            file_watchers_used: Arc::new(Mutex::new(HashSet::new())),
            file_observations: Arc::new(Mutex::new(HashMap::new())),
            resource_provider: Arc::new(RegisteredResourceProvider),
            faults: Mutex::new(None),
            unrecorded_faults: Mutex::new(Vec::new()),
            fault_injection: None,
            cleanup_deadline: CLEANUP_DEADLINE,
            #[cfg(test)]
            raised_faults: Mutex::new(Some(Vec::new())),
        })
    }
}

impl<R: RuntimeControl> Reconciler<R> {
    pub fn new(store: Arc<Store>, runtime: Arc<R>, host: String, notify: Arc<Notify>) -> Self {
        Self {
            store,
            runtime,
            host,
            endpoint: "unused-test-endpoint".into(),
            driver_state_dir: std::env::temp_dir().join("st3-test-drivers"),
            runtime_environment: BTreeMap::new(),
            notify,
            event_notify: watch::channel(0_u64).0,
            armed_schedules: Arc::new(Mutex::new(std::collections::HashSet::new())),
            armed_observers: Arc::new(Mutex::new(std::collections::HashSet::new())),
            gate_poll_armed: Arc::new(AtomicBool::new(false)),
            observer_deadlines: Arc::new(Mutex::new(HashMap::new())),
            observer_cursors: Arc::new(Mutex::new(HashMap::new())),
            delayed_restarts: Arc::new(Mutex::new(HashMap::new())),
            checkout_retries: Arc::new(Mutex::new(HashMap::new())),
            declared_checkouts: Mutex::new(HashMap::new()),
            materialized_mission_generations: Mutex::new(BTreeSet::new()),
            retired_predecessor_generations: Mutex::new(BTreeSet::new()),
            #[cfg(test)]
            mission_declaration_parses: std::sync::atomic::AtomicUsize::new(0),
            file_watchers: Arc::new(Mutex::new(HashMap::new())),
            file_watchers_used: Arc::new(Mutex::new(HashSet::new())),
            file_observations: Arc::new(Mutex::new(HashMap::new())),
            resource_provider: Arc::new(RegisteredResourceProvider),
            faults: Mutex::new(None),
            unrecorded_faults: Mutex::new(Vec::new()),
            fault_injection: None,
            cleanup_deadline: CLEANUP_DEADLINE,
            #[cfg(test)]
            raised_faults: Mutex::new(Some(Vec::new())),
        }
    }

    #[doc(hidden)]
    pub fn with_fault_injection(mut self, injection: Arc<dyn FaultInjection>) -> Self {
        self.fault_injection = Some(injection);
        self
    }

    #[doc(hidden)]
    pub fn with_cleanup_deadline(mut self, deadline: Duration) -> Self {
        self.cleanup_deadline = deadline;
        self
    }

    /// Let passes raise faults, for a unit test of fault isolation itself.
    #[cfg(test)]
    fn tolerating_faults(self) -> Self {
        *self.raised_faults.lock().unwrap() = None;
        self
    }

    #[cfg(test)]
    fn with_resource_provider(mut self, provider: Arc<dyn ResourceProvider>) -> Self {
        self.resource_provider = provider;
        self
    }

    #[cfg(test)]
    fn with_event_notify(mut self, event_notify: watch::Sender<u64>) -> Self {
        self.event_notify = event_notify;
        self
    }

    pub async fn run(self: Arc<Self>) {
        self.notify.notify_one();
        // When the last pass began, and whether it changed nothing.
        let mut quiet_pass_started = None;
        loop {
            match self.blocking(|this| this.next_reconcile_deadline()).await {
                Some(deadline) => {
                    let delay = deadline_sleep_ms(deadline, now_ms(), quiet_pass_started);
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    }
                }
                None => self.notify.notified().await,
            }
            for pass in 0..64 {
                let started = now_ms();
                let changed = self
                    .blocking(|this| {
                        let before = this.store.index().ok();
                        if let Err(error) = this.reconcile_once() {
                            let _ = this.record_once(
                                &format!("daemon/{}", this.host),
                                "daemon.diagnostic",
                                BTreeMap::from([
                                    ("severity".into(), Value::String("error".into())),
                                    ("code".into(), Value::String("reconcile-failed".into())),
                                    ("status".into(), Value::String("unreachable".into())),
                                    ("reason".into(), Value::String(error.to_string())),
                                ]),
                            );
                        }
                        before != this.store.index().ok()
                    })
                    .await;
                self.event_notify
                    .send_modify(|generation| *generation = generation.saturating_add(1));
                quiet_pass_started = (!changed).then_some(started);
                if !changed {
                    break;
                }
                if pass == 63 {
                    self.notify.notify_one();
                } else {
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    /// Run blocking reconciler work on the blocking pool. A pass reads and writes the store and
    /// can wait for its writer; inline, that would hold an async worker that the API, timers and
    /// health checks need. A panic resumes here, so the supervisor still restarts the reconciler.
    async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> T {
        let this = self.clone();
        match tokio::task::spawn_blocking(move || work(&this)).await {
            Ok(value) => value,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }

    /// Run the reconciler and start it again if it panics. A panic ends only the task, so without
    /// this the daemon would keep serving its API with nothing reconciling the host.
    pub async fn supervise(self: Arc<Self>) {
        let mut delay = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            let Err(error) = tokio::spawn(self.clone().run()).await else {
                return;
            };
            if !error.is_panic() {
                return;
            }
            let reason = panic_message(error.into_panic().as_ref());
            let _ = self.record_once(
                &format!("daemon/{}", self.host),
                "daemon.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("code".into(), Value::String("reconciler-panicked".into())),
                    ("status".into(), Value::String("restarting".into())),
                    ("reason".into(), Value::String(reason)),
                ]),
            );
            if started.elapsed() > Duration::from_secs(60) {
                delay = Duration::from_secs(1);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    }

    fn signal_changed(&self) {
        signal_changed(&self.notify, &self.event_notify);
    }

    /// Reconcile one item of the pass on its own. An error or a panic is recorded as a fault on
    /// `subject` in `scope`, and the pass carries on with every other item. The next success
    /// records the item's recovery.
    fn isolate<T>(
        &self,
        scope: &str,
        subject: &str,
        item: impl FnOnce() -> Result<T>,
    ) -> Option<T> {
        let result = caught(|| {
            if let Some(reason) = self
                .fault_injection
                .as_ref()
                .and_then(|injection| injection.fault(scope, subject))
            {
                anyhow::bail!(reason);
            }
            item()
        });
        let (value, outcome) = match result {
            Ok(value) => (Some(value), Ok(())),
            Err(error) => (None, Err(error)),
        };
        #[cfg(test)]
        if let (Err(error), Some(raised)) = (&outcome, self.raised_faults.lock().unwrap().as_mut())
        {
            raised.push(format!("{subject} {scope}: {error:#}"));
        }
        if let Err(error) = self.record_fault(subject, scope, outcome) {
            self.unrecorded_faults
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("{subject} {scope}: {error:#}"));
        }
        value
    }

    /// Record a fault when it first appears or its cause changes, and its recovery once.
    fn record_fault(&self, subject: &str, scope: &str, outcome: Result<()>) -> Result<()> {
        match outcome {
            Err(error) => {
                let reason = format!("{error:#}");
                let mut faults = self.open_faults()?;
                let open = faults.get_or_insert_with(BTreeMap::new);
                let key = (subject.to_owned(), scope.to_owned());
                if open.get(&key) == Some(&reason) {
                    return Ok(());
                }
                self.append_fault(subject, scope, "faulted", &reason)?;
                open.insert(key, reason);
                Ok(())
            }
            Ok(()) => self.close_fault(subject, scope, "the item reconciled successfully"),
        }
    }

    fn close_fault(&self, subject: &str, scope: &str, reason: &str) -> Result<()> {
        let mut faults = self.open_faults()?;
        let open = faults.get_or_insert_with(BTreeMap::new);
        let key = (subject.to_owned(), scope.to_owned());
        if !open.contains_key(&key) {
            return Ok(());
        }
        self.append_fault(subject, scope, "recovered", reason)?;
        open.remove(&key);
        Ok(())
    }

    fn open_faults(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<BTreeMap<(String, String), String>>>> {
        let mut faults = self.faults.lock().unwrap_or_else(PoisonError::into_inner);
        if faults.is_none() {
            *faults = Some(self.store.open_reconcile_faults(&self.host)?);
        }
        Ok(faults)
    }

    fn append_fault(&self, subject: &str, scope: &str, status: &str, reason: &str) -> Result<()> {
        self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "reconcile.fault".into(),
            actor: None,
            fields: BTreeMap::from([
                ("scope".into(), Value::String(scope.into())),
                ("status".into(), Value::String(status.into())),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        Ok(())
    }

    /// The earliest time the reconciler must wake. Each deadline source is read on its own. One
    /// that fails records a fault on the daemon and asks to be read again shortly, so the other
    /// sources keep their deadlines.
    fn next_reconcile_deadline(&self) -> Option<u128> {
        let daemon = format!("daemon/{}", self.host);
        let retry = now_ms().saturating_add(DEADLINE_SOURCE_RETRY_MS);
        let read = |scope: &str, source: &dyn Fn() -> Result<Option<u128>>| {
            self.isolate(scope, &daemon, source).unwrap_or(Some(retry))
        };
        [
            read("deadline/missions", &|| {
                self.store.next_active_mission_deadline(&self.host)
            }),
            read("deadline/work-wakes", &|| self.next_work_wake_deadline()),
            read("deadline/provider-capacity-retries", &|| {
                self.next_provider_capacity_retry_deadline()
            }),
            read("deadline/subscription-retries", &|| {
                self.store.next_subscription_mission_retry_deadline()
            }),
            self.delayed_restarts
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .values()
                .copied()
                .min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn next_work_wake_deadline(&self) -> Result<Option<u128>> {
        let mut work = self.store.work_for_reconcile_all()?;
        let local_agents = self
            .store
            .desired_subjects()?
            .into_iter()
            .filter(|subject| {
                subject.kind == "agent"
                    && subject
                        .member
                        .as_ref()
                        .is_some_and(|member| member.host == self.host)
            })
            .map(|subject| subject.subject)
            .collect::<BTreeSet<_>>();
        let run_orders = self.wake_run_orders(&local_agents, &work)?;
        let candidates = local_agents
            .iter()
            .filter_map(|agent| {
                let order = run_orders.get(agent).map(Vec::as_slice).unwrap_or_default();
                next_work_wake_for_agent(agent, &work, order).map(str::to_owned)
            })
            .collect::<BTreeSet<_>>();
        let now = now_ms();
        for step in &mut work {
            if candidates.contains(step.subject.as_str()) {
                // A step whose wake cannot be read keeps no wake, so it loses only its own
                // deadline.
                let subject = step.subject.clone();
                self.isolate("wake-deadline", &subject, || {
                    self.store.populate_work_wake_for_reconcile(step, now)
                });
            }
        }
        Ok(work_wake_deadline(&work, &local_agents, &run_orders, now))
    }

    /// Seat orders for the local seats whose next wake depends on run order.
    /// Reading an order replays the seat's moves, so a seat that holds work, has
    /// ready work in at most one run, or cannot be woken is skipped.
    fn wake_run_orders(
        &self,
        agents: &BTreeSet<String>,
        work: &[StepRunView],
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut choosing = BTreeSet::new();
        for agent in agents {
            if seat_chooses_between_runs(agent, work)
                && self.store.current_harness(agent)?.is_some_and(|harness| {
                    matches!(harness.state.as_str(), "ready" | "working" | "idle")
                })
            {
                choosing.insert(agent.as_str());
            }
        }
        if choosing.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut orders = self.store.seat_run_orders()?;
        orders.retain(|agent, _| choosing.contains(agent.as_str()));
        Ok(orders)
    }

    /// A request that declared `until` closes on its own once every target meets that `trace
    /// wait` condition. The host that accepted the request evaluates it, so it closes once.
    fn resolve_attention_whose_until_holds(&self) -> Result<()> {
        for request in self.store.pending_attention_with_until(&self.host)? {
            let Some(until) = request.until.as_deref() else {
                continue;
            };
            let mut holds = !request.targets.is_empty();
            for target in &request.targets {
                let status = self.store.status_at(Some(target), None, None)?;
                if !crate::model::status_wait_condition_holds(until, status.subjects.first()) {
                    holds = false;
                    break;
                }
            }
            if holds {
                self.store.resolve_attention_automatically(
                    &request.subject,
                    &format!("every target is {until}"),
                    &format!("{}:until", request.request),
                )?;
                self.signal_changed();
            }
        }
        Ok(())
    }

    fn next_provider_capacity_retry_deadline(&self) -> Result<Option<u128>> {
        let mut deadline = None;
        for subject in self
            .store
            .desired_subjects()?
            .into_iter()
            .filter(|subject| {
                subject.kind == "agent"
                    && subject
                        .member
                        .as_ref()
                        .is_some_and(|member| member.host == self.host)
            })
        {
            for claim in self
                .store
                .claims_for(&subject.subject, Some("harness.diagnostic"))?
            {
                let fields = claim.body.get("fields").unwrap_or(&claim.body);
                if fields.get("code").and_then(Value::as_str) != Some("provider-capacity")
                    || fields.get("status").and_then(Value::as_str) != Some("waiting")
                    || self
                        .store
                        .operation_claim(&provider_capacity_retry_key(&claim.id))?
                        .is_some()
                {
                    continue;
                }
                let Some(due) = fields.get("retry_after_unix_ms").and_then(Value::as_u64) else {
                    continue;
                };
                deadline = Some(deadline.map_or(u128::from(due), |current: u128| {
                    current.min(u128::from(due))
                }));
            }
        }
        Ok(deadline)
    }

    pub fn reconcile_once(&self) -> Result<()> {
        let mut desired = self.store.desired_subjects()?;
        let terminal_owned = self.store.terminal_owned_runtime_subjects()?;
        for subject in &mut desired {
            if terminal_owned.contains(&subject.subject)
                && subject
                    .member
                    .as_ref()
                    .is_some_and(|member| member.host == self.host)
            {
                subject.kind = "stop".into();
            }
        }
        let ptys = match self.runtime.snapshot_ptys() {
            Ok(snapshot) => Some(
                snapshot
                    .into_iter()
                    .map(|item| (item.runtime_id.clone(), item))
                    .collect::<HashMap<_, _>>(),
            ),
            Err(error) => {
                // An unavailable snapshot is unknown, not an empty runtime set. Treating it as
                // empty could finish run cleanup while a PTY lives, so terminal members wait for
                // the next snapshot. Every other part of the pass still runs.
                self.arm_restart("runtime-snapshot", now_ms().saturating_add(1_000));
                self.record_once(
                    &format!("daemon/{}", self.host),
                    "daemon.diagnostic",
                    BTreeMap::from([
                        ("severity".into(), Value::String("error".into())),
                        (
                            "code".into(),
                            Value::String("runtime-snapshot-failed".into()),
                        ),
                        ("status".into(), Value::String("indeterminate".into())),
                        ("reason".into(), Value::String(error.to_string())),
                    ]),
                )?;
                None
            }
        };

        let active = desired.iter().collect::<Vec<_>>();
        let mut member_errors = BTreeMap::new();
        for subject in &active {
            if subject.kind == "stop" {
                continue;
            }
            let Some(member) = subject
                .member
                .as_ref()
                .filter(|member| member.host == self.host)
            else {
                continue;
            };
            let workspace = Path::new(&member.workspace);
            if workspace.is_dir() {
                continue;
            }
            let checkout = (subject.kind == "agent")
                .then(|| Checkout::from_desired(&subject.desired))
                .flatten();
            let result = if let Some(checkout) = checkout {
                self.create_checkout(&subject.subject, &checkout, workspace)
                    .and_then(|failure| match failure {
                        Some(reason) => Err(anyhow::anyhow!(reason)),
                        None => Ok(()),
                    })
            } else if member.workspace_create {
                fs::create_dir_all(workspace).map_err(anyhow::Error::from)
            } else {
                Err(anyhow::anyhow!(
                    "the workspace does not exist and create was not requested"
                ))
            };
            if let Err(error) = result.with_context(|| format!("workspace {}", workspace.display()))
            {
                member_errors.insert(subject.subject.clone(), error);
            }
        }
        let renderable = active
            .iter()
            .copied()
            .filter(|subject| !member_errors.contains_key(&subject.subject))
            .collect::<Vec<_>>();
        // A render panic faults this host's members; stops never render, so they still run.
        let rendered = std::panic::catch_unwind(AssertUnwindSafe(|| {
            crate::render::apply_all(&self.store, &renderable, &self.host)
        }))
        .unwrap_or_else(|panic| {
            let reason = panic_message(panic.as_ref());
            renderable
                .iter()
                .filter(|subject| {
                    subject.kind != "stop"
                        && subject
                            .member
                            .as_ref()
                            .is_some_and(|member| member.host == self.host)
                })
                .map(|subject| {
                    (
                        subject.subject.clone(),
                        Err(anyhow::anyhow!("render panicked: {reason}")),
                    )
                })
                .collect()
        });
        let agents = renderable
            .iter()
            .filter(|subject| subject.kind == "agent")
            .map(|subject| subject.subject.as_str())
            .collect::<BTreeSet<_>>();
        for (subject, result) in rendered {
            let result = result.and_then(|result| {
                let mut applied = BTreeMap::new();
                // Only an agent has a harness to report a diagnostic on. Another member keeps its
                // warnings on its render receipt, so a warning never faults it.
                if agents.contains(subject.as_str()) {
                    for warning in result.warnings {
                        self.record_once(
                            &subject,
                            "harness.diagnostic",
                            BTreeMap::from([
                                ("status".into(), Value::String("warning".into())),
                                ("reason".into(), Value::String(warning)),
                            ]),
                        )?;
                    }
                } else if !result.warnings.is_empty() {
                    applied.insert("warnings".into(), serde_json::to_value(result.warnings)?);
                }
                if !result.receipts.is_empty() {
                    applied.insert("writes".into(), serde_json::to_value(result.receipts)?);
                }
                if !applied.is_empty() {
                    self.record_once(&subject, "render.applied", applied)?;
                }
                Ok(())
            });
            if let Err(error) = result {
                member_errors.insert(subject, error);
            }
        }
        // A later run can declare the same workspace, so a finished run never removes a
        // checkout that a current member on this host still uses.
        let live_workspaces = active
            .iter()
            .filter(|subject| subject.kind != "stop")
            .filter_map(|subject| subject.member.as_ref())
            .filter(|member| member.host == self.host)
            .map(|member| member.workspace.as_str())
            .collect::<BTreeSet<_>>();
        let mut work_message_agents = Vec::new();
        let mut deferred_member_faults = BTreeMap::new();
        let mut diagnostic_errors = Vec::new();
        self.record_unreadable_members(&active, &mut diagnostic_errors);
        for subject in &active {
            let owner = if let Some(member) = &subject.member {
                Ok(Some(member.host.clone()))
            } else if subject.kind == "stop" {
                self.store
                    .selected_actual_origin(&subject.subject)
                    .and_then(|origin| {
                        Ok(origin.or(self.store.selected_desired_origin(&subject.subject)?))
                    })
            } else {
                Ok(None)
            };
            match owner {
                Ok(Some(owner)) if owner == self.host => {}
                Ok(_) => continue,
                Err(error) => {
                    diagnostic_errors.push(format!(
                        "{}: determine member owner: {error:#}",
                        subject.subject
                    ));
                    continue;
                }
            }
            let result = caught(|| -> Result<()> {
                // A workspace or render failure blocks only a start or restart. A running member
                // is still observed, checked, and given its work.
                let mut blocked = member_errors.remove(&subject.subject);
                if subject.kind == "stop" {
                    self.reconcile_stop(subject, ptys.as_ref())?;
                    self.remove_checkout_after_run(subject, &live_workspaces)?;
                    return Ok(());
                }
                let Some(member) = &subject.member else {
                    return Ok(());
                };
                if member.host != self.host {
                    return Ok(());
                }
                let observed = if member.terminal {
                    // An unavailable snapshot is unknown, not an empty runtime set, so a terminal
                    // member is neither started nor judged until the snapshot returns.
                    let Some(ptys) = ptys.as_ref() else {
                        return blocked.map_or(Ok(()), Err);
                    };
                    ptys.get(&member.runtime_id).cloned()
                } else {
                    self.runtime.observe_exec(&member.runtime_id)?
                };
                match observed {
                    Some(observation) if observation.status == "running" => {
                        self.record_member(subject, &observation, true)?;
                        self.reconcile_claude_auth_screen(subject, member, &observation)?;
                        self.reconcile_claude_trust_screen(
                            subject,
                            member,
                            &observation,
                            now_ms(),
                        )?;
                        self.reconcile_driver_readiness(subject, member, &observation, now_ms())?;
                        if subject.kind == "agent"
                            && let Some(incarnation) = observation.incarnation_id.as_deref()
                        {
                            work_message_agents.push((
                                subject.subject.clone(),
                                incarnation.to_owned(),
                                member.clone(),
                            ));
                        }
                    }
                    Some(observation)
                        if matches!(observation.status.as_str(), "exited" | "vanished") =>
                    {
                        self.record_member(subject, &observation, false)?;
                        if !self.member_was_launched_for_selected_desired(&subject.subject)? {
                            if let Some(error) = blocked.take() {
                                return Err(error);
                            }
                            self.perform_start(
                                subject,
                                member,
                                "the desired member revision changed",
                            )?;
                            return Ok(());
                        }
                        let restart = match member.restart {
                            RestartType::Always => true,
                            RestartType::OnFailure => observation.exit_code != Some(0),
                            RestartType::Never => false,
                        };
                        // A trust-prompt recovery stopped this incarnation in order to replace it,
                        // whatever the member's own exit policy says.
                        let recovering = self
                            .claude_trust_recovery_stopped(&subject.subject, &observation)?
                            || self
                                .fresh_context_recovery_stopped(&subject.subject, &observation)?;
                        if (restart || recovering) && member.lifecycle == MemberLifecycle::Service {
                            if let Some(error) = blocked.take() {
                                return Err(error);
                            }
                            self.reconcile_restart(subject, member, &observation)?;
                        }
                    }
                    Some(observation) => {
                        self.record_member(subject, &observation, false)?;
                    }
                    None if member.lifecycle == MemberLifecycle::AdoptOnly => {
                        self.record_once(
                            &subject.subject,
                            "runtime.observed",
                            member_fields(member, "absent", None, false),
                        )?;
                    }
                    None => {
                        let prior = self.store.latest_actual_value(&subject.subject)?;
                        if prior.is_some()
                            && !self.member_was_launched_for_selected_desired(&subject.subject)?
                        {
                            if let Some(error) = blocked.take() {
                                return Err(error);
                            }
                            self.perform_start(
                                subject,
                                member,
                                "the desired member revision changed",
                            )?;
                            return Ok(());
                        }
                        if prior.as_ref().is_some_and(|actual| {
                            matches!(
                                actual_field(actual, "status").and_then(Value::as_str),
                                Some(
                                    "running"
                                        | "ready"
                                        | "working"
                                        | "idle"
                                        | "starting"
                                        | "exited"
                                        | "vanished"
                                )
                            )
                        }) {
                            let observation = RuntimeObservation {
                                runtime_id: member.runtime_id.clone(),
                                terminal: member.terminal,
                                status: "vanished".into(),
                                exit_code: prior
                                    .as_ref()
                                    .and_then(|actual| actual_field(actual, "exit_code"))
                                    .and_then(Value::as_i64),
                                incarnation_id: prior
                                    .as_ref()
                                    .and_then(|actual| actual_field(actual, "incarnation_id"))
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            };
                            self.record_member(subject, &observation, false)?;
                            let restart = match member.restart {
                                RestartType::Always => true,
                                RestartType::OnFailure => observation.exit_code != Some(0),
                                RestartType::Never => false,
                            };
                            if restart
                                || self.fresh_context_recovery_stopped(
                                    &subject.subject,
                                    &observation,
                                )?
                            {
                                if let Some(error) = blocked.take() {
                                    return Err(error);
                                }
                                self.reconcile_restart(subject, member, &observation)?;
                            }
                        } else {
                            if let Some(error) = blocked.take() {
                                return Err(error);
                            }
                            self.perform_start(subject, member, "the desired member is absent")?;
                        }
                    }
                }
                blocked.map_or(Ok(()), Err)
            });
            // A running agent's member pass also includes deferred work delivery, so its result
            // is recorded with that delivery.
            let deferred = work_message_agents
                .last()
                .is_some_and(|(agent, _, _)| agent == &subject.subject);
            if deferred {
                if let Err(error) = result {
                    deferred_member_faults.insert(subject.subject.clone(), error);
                }
            } else if let Err(error) = self.record_member_reconcile_result(&subject.subject, result)
            {
                diagnostic_errors.push(format!("{}: {error:#}", subject.subject));
            }
        }
        // Each later stage runs on its own. A stage that fails records a fault on this daemon and
        // the stages after it still run, so no intake item can hold back mission evaluation, run
        // cleanup, or work delivery on this host.
        let daemon = format!("daemon/{}", self.host);
        // Intake left by a terminal owner or a superseded generation must not observe, deliver,
        // or start work. A stopped declaration still runs so it can settle its own state.
        let intake = self.isolate("stage/intake", &daemon, || {
            let retired_intake = self.store.retired_owned_intake_subjects()?;
            Ok(desired
                .iter()
                .filter(|subject| {
                    matches!(
                        subject.kind.as_str(),
                        "observer" | "subscription" | "schedule"
                    )
                })
                .filter(|subject| {
                    !retired_intake.contains(&subject.subject)
                        || intake_is_stopped(subject, &self.host)
                })
                .cloned()
                .collect::<Vec<_>>())
        });
        if let Some(intake) = intake {
            self.isolate("stage/observers", &daemon, || {
                self.reconcile_resource_observers(&intake)
            });
            self.isolate("stage/schedules", &daemon, || {
                self.reconcile_schedules(&intake)
            });
            self.isolate("stage/scheduled-work", &daemon, || {
                self.reconcile_scheduled_work(&intake)
            });
            self.isolate("stage/subscriptions", &daemon, || {
                self.reconcile_subscription_missions(&intake)
            });
        }
        self.isolate("stage/provider-capacity-retries", &daemon, || {
            self.reconcile_provider_capacity_retries(&desired)
        });
        self.isolate("stage/retired-agent-attention", &daemon, || {
            self.resolve_attention_for_retired_agents(&desired)
        });
        self.file_watchers_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.isolate("stage/missions", &daemon, || self.evaluate_mission_runs());
        self.release_unused_file_watchers();
        self.isolate("stage/attention-until", &daemon, || {
            self.resolve_attention_whose_until_holds()
        });
        // Mission state is the primary control-plane projection. Evaluate it before
        // wake-message bookkeeping so a large mailbox or work history cannot starve
        // newly-created runs of their first readiness pass.
        for (agent, incarnation, member) in work_message_agents {
            let result =
                caught(|| self.reconcile_work_messages(&agent, &incarnation, Some(&member)));
            let result = match deferred_member_faults.remove(&agent) {
                Some(error) => Err(error),
                None => result,
            };
            if let Err(error) = self.record_member_reconcile_result(&agent, result) {
                diagnostic_errors.push(format!("{agent}: {error:#}"));
            }
        }
        diagnostic_errors.append(
            &mut self
                .unrecorded_faults
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if !diagnostic_errors.is_empty() {
            // Each of these items was already skipped on its own and the pass carried on. Only
            // the record of its fault is missing, so the host is faulted, not unreachable.
            let reason = format!("unrecorded item faults: {}", diagnostic_errors.join("; "));
            #[cfg(test)]
            if let Some(raised) = self.raised_faults.lock().unwrap().as_mut() {
                raised.push(reason.clone());
            }
            self.record_once(
                &daemon,
                "daemon.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("code".into(), Value::String("fault-record-failed".into())),
                    ("status".into(), Value::String("faulted".into())),
                    ("reason".into(), Value::String(reason)),
                ]),
            )?;
        }
        #[cfg(test)]
        if let Some(raised) = self.raised_faults.lock().unwrap().as_mut() {
            let raised = std::mem::take(raised);
            anyhow::ensure!(raised.is_empty(), "reconcile faults: {}", raised.join("; "));
        }
        Ok(())
    }

    /// A member declaration this build cannot read has no member, so the member loop would
    /// pass over it without a word: never observed, started or stopped. The host that published
    /// it records the fault on it instead, until a build that can read it takes it up.
    fn record_unreadable_members(
        &self,
        active: &[&DesiredSubject],
        diagnostic_errors: &mut Vec<String>,
    ) {
        let candidates = active
            .iter()
            .filter(|subject| subject.member.is_none() && subject.kind != "stop")
            .map(|subject| subject.subject.as_str())
            .collect::<Vec<_>>();
        let unreadable = match self.store.unreadable_members(&candidates) {
            Ok(unreadable) => unreadable,
            Err(error) => {
                diagnostic_errors.push(format!("read member declarations: {error:#}"));
                return;
            }
        };
        for (subject, reason) in unreadable {
            let result = self
                .store
                .selected_desired_origin(&subject)
                .and_then(|origin| {
                    if origin.as_deref() != Some(self.host.as_str()) {
                        return Ok(());
                    }
                    self.record_member_reconcile_result(
                        &subject,
                        Err(anyhow::anyhow!(
                            "this build cannot read the member declaration: {reason}"
                        )),
                    )
                });
            if let Err(error) = result {
                diagnostic_errors.push(format!("{subject}: {error:#}"));
            }
        }
    }

    fn record_member_reconcile_result(&self, subject: &str, result: Result<()>) -> Result<()> {
        let previous = self.store.member_reconcile_fault(subject, None)?;
        let (decision, reason) = match result {
            Err(error) => {
                let reason = format!("{error:#}");
                if previous.as_deref() == Some(reason.as_str()) {
                    return Ok(());
                }
                ("member-fault", reason)
            }
            Ok(()) if previous.is_some() => (
                "member-recovered",
                "the member reconciled successfully".to_owned(),
            ),
            Ok(()) => return Ok(()),
        };
        self.record_once(
            subject,
            "runtime.reconcile-decision",
            BTreeMap::from([
                ("key".into(), Value::String("member-reconcile".into())),
                ("decision".into(), Value::String(decision.into())),
                ("reason".into(), Value::String(reason)),
            ]),
        )?;
        Ok(())
    }

    fn reconcile_provider_capacity_retries(&self, desired: &[DesiredSubject]) -> Result<()> {
        let now = now_ms();
        for subject in desired.iter().filter(|subject| {
            subject.kind == "agent"
                && subject
                    .member
                    .as_ref()
                    .is_some_and(|member| member.host == self.host)
        }) {
            for claim in self
                .store
                .claims_for(&subject.subject, Some("harness.diagnostic"))?
            {
                let fields = claim.body.get("fields").unwrap_or(&claim.body);
                if fields.get("code").and_then(Value::as_str) != Some("provider-capacity")
                    || fields.get("status").and_then(Value::as_str) != Some("waiting")
                {
                    continue;
                }
                let Some(due) = fields.get("retry_after_unix_ms").and_then(Value::as_u64) else {
                    continue;
                };
                if u128::from(due) > now {
                    continue;
                }
                let retry_key = provider_capacity_retry_key(&claim.id);
                if self.store.operation_claim(&retry_key)?.is_some() {
                    continue;
                }
                let incarnation = fields
                    .get("incarnation_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let same_capacity_session = self
                    .store
                    .current_harness(&subject.subject)?
                    .is_some_and(|harness| {
                        harness.incarnation_id == incarnation
                            && harness.state == "idle"
                            && harness.reason.as_deref() == Some("providerCapacity")
                    });
                if !same_capacity_session {
                    self.store.append_claim(&ClaimInput {
                        subject: subject.subject.clone(),
                        kind: "publication.operation".into(),
                        actor: None,
                        fields: BTreeMap::from([
                            (
                                "operation".into(),
                                Value::String("provider-capacity-retry".into()),
                            ),
                            (
                                "action".into(),
                                Value::String("skip-stale-incarnation".into()),
                            ),
                            ("status".into(), Value::String("accepted".into())),
                        ]),
                        evidence: vec![claim.id],
                        expected_subject: None,
                        idempotency_key: Some(retry_key),
                    })?;
                    self.signal_changed();
                    continue;
                }
                let attempt = fields
                    .get("retry_attempt")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                let step = fields.get("step_run").and_then(Value::as_str);
                let message_id = &hex::encode(sha2::Sha256::digest(retry_key.as_bytes()))[..16];
                let message_subject = format!("message/{message_id}");
                let work_context = step
                    .map(|step| format!(" Continue the claimed work `{step}`."))
                    .unwrap_or_default();
                self.store.append_claim(&ClaimInput {
                    subject: message_subject,
                    kind: "message.sent".into(),
                    actor: Some("daemon/runtime".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("daemon/runtime".into())),
                        ("to".into(), Value::String(subject.subject.clone())),
                        (
                            "content".into(),
                            Value::String(format!(
                                "The provider-capacity backoff elapsed.{work_context} Resume in this existing session; do not create a replacement worker."
                            )),
                        ),
                        (
                            "status".into(),
                            Value::String("sent".into()),
                        ),
                        (
                            "title".into(),
                            Value::String(format!(
                                "Provider capacity retry {attempt}"
                            )),
                        ),
                        ("in_reply_to".into(), Value::Null),
                        (
                            "tags".into(),
                            Value::Array(vec![
                                Value::String("st3-provider-capacity-retry".into()),
                                Value::String(format!("st3-retry-attempt:{attempt}")),
                                Value::String(format!("diagnostic-claim:{}", claim.id)),
                            ]),
                        ),
                    ]),
                    evidence: vec![claim.id],
                    expected_subject: None,
                    idempotency_key: Some(retry_key),
                })?;
                self.signal_changed();
            }
        }
        Ok(())
    }

    fn reconcile_driver_readiness(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: &RuntimeObservation,
        now: u128,
    ) -> Result<()> {
        if subject.kind != "agent" {
            return Ok(());
        }
        let Some(driver) = member.driver.as_deref() else {
            return Ok(());
        };
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(());
        };
        let attention_key = format!("harness-readiness:{0}:{incarnation}", subject.subject);
        let attention_digest = hex::encode(sha2::Sha256::digest(attention_key.as_bytes()));
        let attention_subject = format!("attention/{}", &attention_digest[..32]);
        let harness_ready = self
            .store
            .current_harness(&subject.subject)?
            .is_some_and(|harness| harness.incarnation_id == incarnation && harness.is_ready());
        if harness_ready {
            if driver == "claude" {
                self.resolve_superseded_claude_auth_attention(&subject.subject, incarnation)?;
            }
            self.resolve_recovered_seat_attention(&subject.subject, incarnation)?;
            self.resolve_pending_alert(
                &attention_key,
                "the same runtime incarnation became ready",
            )?;
            return Ok(());
        }

        // This deadline describes startup only. A harness that was ready and later lost
        // observation needs a delivery/health diagnosis, not a false startup failure.
        if self
            .store
            .harness_was_ready(&subject.subject, incarnation)?
        {
            return Ok(());
        }

        let Some(runtime_claim) = self.running_runtime_claim(&subject.subject, incarnation)? else {
            return Ok(());
        };
        let deadline = runtime_claim
            .accepted_at_unix_ms
            .saturating_add(HARNESS_READINESS_DEADLINE_MS);
        if now < deadline {
            self.arm_restart(
                &format!("readiness:{}:{incarnation}", subject.subject),
                deadline,
            );
            return Ok(());
        }

        let reason = format!(
            "the {driver} harness did not become ready within {} seconds",
            HARNESS_READINESS_DEADLINE_MS / 1_000
        );
        let deadline_recorded = self
            .store
            .observations_for(&subject.subject, "runtime.readiness-deadline-reached")?
            .iter()
            .any(|claim| {
                claim
                    .body
                    .pointer("/fields/incarnation_id")
                    .and_then(Value::as_str)
                    == Some(incarnation)
            });
        let attention_recorded = self.store.attention_request(&attention_subject)?.is_some();
        let mut changed = false;
        if !deadline_recorded {
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "runtime.readiness-deadline-reached".into(),
                actor: None,
                fields: BTreeMap::from([
                    (
                        "runtime_id".into(),
                        Value::String(observation.runtime_id.clone()),
                    ),
                    ("driver".into(), Value::String(driver.into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "deadline_unix_ms".into(),
                        Value::String(deadline.to_string()),
                    ),
                    ("reason".into(), Value::String(reason.clone())),
                ]),
                evidence: vec![runtime_claim.id],
                expected_subject: None,
                idempotency_key: Some(format!("{attention_key}:deadline")),
            })?;
            changed = true;
        }
        if !attention_recorded {
            self.store.request_attention(
                &attention_subject,
                &AttentionRequest {
                    reviewer: "person/operator".into(),
                    title: "An agent harness did not become ready".into(),
                    reason,
                    severity: "error".into(),
                    targets: vec![subject.subject.clone()],
                    actor: "agent/st3/reconciler".into(),
                    idempotency_key: format!("{attention_key}:requested"),
                },
            )?;
            changed = true;
        }
        if changed {
            self.signal_changed();
        }
        Ok(())
    }

    fn resolve_superseded_claude_auth_attention(
        &self,
        subject: &str,
        current_incarnation: &str,
    ) -> Result<()> {
        for claim in self.store.claims_for(subject, Some("harness.diagnostic"))? {
            if claim.body.pointer("/fields/code").and_then(Value::as_str)
                != Some("provider-auth-expired")
            {
                continue;
            }
            let Some(old_incarnation) = claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str)
            else {
                continue;
            };
            if old_incarnation == current_incarnation {
                continue;
            }
            self.resolve_pending_alert(
                &format!("claude-auth-expired:{subject}:{old_incarnation}"),
                "a new Claude runtime incarnation became ready",
            )?;
        }
        Ok(())
    }

    /// Startup and crash-loop alerts name one incarnation or desired token. Once another
    /// incarnation of the seat is ready, those alerts describe a runtime that is gone.
    fn resolve_recovered_seat_attention(
        &self,
        subject: &str,
        current_incarnation: &str,
    ) -> Result<()> {
        for claim in self
            .store
            .observations_for(subject, "runtime.readiness-deadline-reached")?
        {
            let Some(old_incarnation) = claim_incarnation(&claim) else {
                continue;
            };
            if old_incarnation == current_incarnation {
                continue;
            }
            self.resolve_pending_alert(
                &format!("harness-readiness:{subject}:{old_incarnation}"),
                "a later runtime incarnation became ready",
            )?;
        }
        let token = self
            .store
            .selected_desired_token(subject)?
            .unwrap_or_default();
        for claim in self
            .store
            .claims_for(subject, Some("runtime.reconcile-decision"))?
        {
            let Some(old_token) = claim
                .body
                .pointer("/fields/key")
                .and_then(Value::as_str)
                .and_then(|key| key.strip_prefix("codex-crash-loop:"))
            else {
                continue;
            };
            if old_token == token {
                continue;
            }
            self.resolve_pending_alert(
                &format!("codex-crash-loop:{subject}:{old_token}"),
                "a new desired revision became ready",
            )?;
        }
        Ok(())
    }

    /// Alerts this host raised about an agent that is no longer desired, or is desired stopped,
    /// have nothing left to fix. Alerts with any other target stay with their reviewer.
    fn resolve_attention_for_retired_agents(&self, desired: &[DesiredSubject]) -> Result<()> {
        let active = desired
            .iter()
            .filter(|subject| subject.kind != "stop")
            .map(|subject| subject.subject.as_str())
            .collect::<BTreeSet<_>>();
        for request in self
            .store
            .pending_attention_requests_raised_by("agent/st3/reconciler", &self.host)?
        {
            if request.targets.is_empty()
                || !request
                    .targets
                    .iter()
                    .all(|target| target.starts_with("agent/") && !active.contains(target.as_str()))
            {
                continue;
            }
            self.store.resolve_attention_automatically(
                &request.subject,
                "the agent it names was removed or stopped",
                &format!("{}:agent-retired", request.request),
            )?;
            self.signal_changed();
        }
        Ok(())
    }

    fn resolve_pending_alert(&self, key: &str, reason: &str) -> Result<()> {
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention_subject = format!("attention/{}", &digest[..32]);
        if self
            .store
            .attention_request(&attention_subject)?
            .is_some_and(|attention| attention.status == "pending")
        {
            self.store.resolve_attention_automatically(
                &attention_subject,
                reason,
                &format!("{key}:resolved"),
            )?;
            self.signal_changed();
        }
        Ok(())
    }

    fn reconcile_claude_auth_screen(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: &RuntimeObservation,
    ) -> Result<()> {
        if subject.kind != "agent" || member.driver.as_deref() != Some("claude") || !member.terminal
        {
            return Ok(());
        }
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(());
        };
        // Claude's login prompt does not emit a StopFailure hook. The initialized MCP channel
        // remains alive, so hook-only observation incorrectly reports this session as ready.
        let Ok(screen) = self.runtime.screen(&member.runtime_id) else {
            return Ok(());
        };
        let (fence, key) = self.claude_auth_fence(&subject.subject, incarnation)?;
        let Some(matched_line) = claude_login_expired(&screen) else {
            // The prompt is gone: a person ran /login, or the match was false. Either way the
            // incarnation can take work again, so its fence and the person's request end.
            if let Some(fence) = fence {
                self.store.append_claim(&ClaimInput {
                    subject: subject.subject.clone(),
                    kind: "harness.diagnostic".into(),
                    actor: Some(subject.subject.clone()),
                    fields: BTreeMap::from([
                        ("status".into(), Value::String("authenticated".into())),
                        (
                            "code".into(),
                            Value::String("provider-auth-restored".into()),
                        ),
                        (
                            "reason".into(),
                            Value::String("Claude no longer shows its expired-login prompt".into()),
                        ),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                    ]),
                    evidence: vec![fence],
                    expected_subject: None,
                    idempotency_key: Some(format!("{key}:restored")),
                })?;
                self.resolve_pending_alert(
                    &key,
                    "Claude no longer shows its expired-login prompt",
                )?;
                self.signal_changed();
            }
            return Ok(());
        };
        if fence.is_some() {
            return Ok(());
        }
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention_subject = format!("attention/{}", &digest[..32]);
        if self.store.attention_request(&attention_subject)?.is_some() {
            return Ok(());
        }
        self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "harness.diagnostic".into(),
                actor: Some(subject.subject.clone()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("status".into(), Value::String("unauthenticated".into())),
                    ("code".into(), Value::String("provider-auth-expired".into())),
                    ("reason".into(), Value::String("Claude reports an expired login; a person must run /login in this terminal and restart the harness".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("matched_line".into(), Value::String(matched_line.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.clone()),
        })?;
        self.store.request_attention(&attention_subject, &AttentionRequest {
                reviewer: "person/nathan".into(),
                title: "Claude login expired".into(),
                reason: format!("{} on {} is unauthenticated. Run /login in its terminal, then restart this harness; work delivery is held until a new authenticated incarnation.", subject.subject, self.host),
                severity: "error".into(),
                targets: vec![subject.subject.clone()],
                actor: "agent/st3/reconciler".into(),
                idempotency_key: format!("{key}:attention"),
        })?;
        self.signal_changed();
        Ok(())
    }

    /// The login fence of one Claude incarnation: the claim that fences it, if it is fenced now,
    /// and the key of that fence, or of the next one. A fence lifted once can fence the same
    /// incarnation again, so a fence after a lift is keyed by that lift.
    fn claude_auth_fence(
        &self,
        subject: &str,
        incarnation: &str,
    ) -> Result<(Option<String>, String)> {
        let mut fence = None;
        let mut key = format!("claude-auth-expired:{subject}:{incarnation}");
        for claim in self.store.claims_for(subject, Some("harness.diagnostic"))? {
            if claim
                .body
                .pointer("/fields/incarnation_id")
                .and_then(Value::as_str)
                != Some(incarnation)
            {
                continue;
            }
            match claim.body.pointer("/fields/code").and_then(Value::as_str) {
                Some("provider-auth-expired") => fence = Some(claim.id),
                Some("provider-auth-restored") => {
                    fence = None;
                    key = format!("claude-auth-expired:{subject}:{incarnation}:{}", claim.id);
                }
                _ => {}
            }
        }
        Ok((fence, key))
    }

    /// Claude's workspace trust prompt appears before any hook or channel can report the session,
    /// so the terminal screen is the only evidence of it. The prompt fences the incarnation as not
    /// ready with its reason. Recovery stops that exact incarnation so its replacement's driver
    /// admits the workspace again; nobody types into the terminal. Repeated prompts go to a person.
    fn reconcile_claude_trust_screen(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: &RuntimeObservation,
        now: u128,
    ) -> Result<()> {
        if subject.kind != "agent" || member.driver.as_deref() != Some("claude") || !member.terminal
        {
            return Ok(());
        }
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(());
        };
        let mut prompts = self.claude_trust_prompts(&subject.subject)?;
        if !prompts
            .iter()
            .any(|claim| claim_incarnation(claim) == Some(incarnation))
        {
            // The prompt precedes readiness, and a ready session may be quoting it.
            if self
                .store
                .harness_was_ready(&subject.subject, incarnation)?
            {
                return Ok(());
            }
            let Some(runtime_claim) = self.running_runtime_claim(&subject.subject, incarnation)?
            else {
                return Ok(());
            };
            let Ok(screen) = self.runtime.screen(&member.runtime_id) else {
                return Ok(());
            };
            if !claude_trust_prompt(&screen) {
                let deadline = runtime_claim
                    .accepted_at_unix_ms
                    .saturating_add(HARNESS_READINESS_DEADLINE_MS);
                if now < deadline {
                    // One grid point per interval, so repeated passes share a single timer.
                    let next =
                        (now / CLAUDE_TRUST_SCREEN_RECHECK_MS + 1) * CLAUDE_TRUST_SCREEN_RECHECK_MS;
                    self.arm_restart(&format!("trust-screen:{}", subject.subject), next);
                }
                return Ok(());
            }
            prompts.push(self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "harness.diagnostic".into(),
                actor: Some(subject.subject.clone()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("status".into(), Value::String("blocked".into())),
                    ("code".into(), Value::String("provider-trust-prompt".into())),
                    ("reason".into(), Value::String("Claude is waiting at its workspace trust prompt and cannot accept work; st replaces this incarnation so its driver admits the workspace again".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: vec![runtime_claim.id],
                expected_subject: None,
                idempotency_key: Some(format!(
                    "claude-trust-prompt:{}:{incarnation}",
                    subject.subject
                )),
            })?);
            self.signal_changed();
        }
        let recent = prompts
            .iter()
            .filter(|claim| {
                now.saturating_sub(claim.accepted_at_unix_ms) < CLAUDE_TRUST_RECOVERY_WINDOW_MS
            })
            .count();
        if recent > CLAUDE_TRUST_RECOVERY_ATTEMPTS {
            let key = format!(
                "claude-trust-prompt-repeated:{}:{incarnation}",
                subject.subject
            );
            let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
            let attention_subject = format!("attention/{}", &digest[..32]);
            if self.store.attention_request(&attention_subject)?.is_none() {
                self.store.request_attention(
                    &attention_subject,
                    &AttentionRequest {
                        reviewer: "person/operator".into(),
                        title: "Claude keeps stopping at its workspace trust prompt".into(),
                        reason: format!(
                            "{} on {} reached Claude's workspace trust prompt {recent} times in {} minutes, so st stopped replacing it. Check that its driver can record the workspace trust in the Claude config, then restart the seat.",
                            subject.subject,
                            self.host,
                            CLAUDE_TRUST_RECOVERY_WINDOW_MS / 60_000
                        ),
                        severity: "error".into(),
                        targets: vec![subject.subject.clone()],
                        actor: "agent/st3/reconciler".into(),
                        idempotency_key: format!("{key}:requested"),
                    },
                )?;
                self.signal_changed();
            }
            return Ok(());
        }
        self.reconcile_runtime_stop(
            &subject.subject,
            &member.runtime_id,
            member.terminal,
            Some(incarnation),
            member.shutdown_timeout_ms,
            Some(observation),
        )?;
        Ok(())
    }

    /// Whether a trust-prompt recovery stopped this exited incarnation.
    fn claude_trust_recovery_stopped(
        &self,
        subject: &str,
        observation: &RuntimeObservation,
    ) -> Result<bool> {
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(false);
        };
        if !self
            .claude_trust_prompts(subject)?
            .iter()
            .any(|claim| claim_incarnation(claim) == Some(incarnation))
        {
            return Ok(false);
        }
        Ok(self
            .store
            .observations_for(subject, "runtime.action.requested")?
            .iter()
            .any(|claim| {
                claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("terminate")
                    && claim_incarnation(claim) == Some(incarnation)
            }))
    }

    fn claude_trust_prompts(&self, subject: &str) -> Result<Vec<crate::model::ClaimRecord>> {
        Ok(self
            .store
            .claims_for(subject, Some("harness.diagnostic"))?
            .into_iter()
            .filter(|claim| {
                claim.body.pointer("/fields/code").and_then(Value::as_str)
                    == Some("provider-trust-prompt")
            })
            .collect())
    }

    fn fresh_context_recovery_stopped(
        &self,
        subject: &str,
        observation: &RuntimeObservation,
    ) -> Result<bool> {
        let Some(incarnation) = observation.incarnation_id.as_deref() else {
            return Ok(false);
        };
        Ok(self
            .store
            .observations_for(subject, "runtime.action.requested")?
            .iter()
            .any(|claim| {
                claim.body.pointer("/fields/action").and_then(Value::as_str)
                    == Some("fresh-context")
                    && claim_incarnation(claim) == Some(incarnation)
            }))
    }

    fn running_runtime_claim(
        &self,
        subject: &str,
        incarnation: &str,
    ) -> Result<Option<crate::model::ClaimRecord>> {
        Ok(self
            .store
            .claims_for(subject, Some("runtime.observed"))?
            .into_iter()
            .rev()
            .find(|claim| {
                claim_incarnation(claim) == Some(incarnation)
                    && claim.body.pointer("/fields/status").and_then(Value::as_str)
                        == Some("running")
            }))
    }

    fn prepare_fresh_context(
        &self,
        agent: &str,
        incarnation: &str,
        step: &StepRunView,
        member: &MemberSpec,
    ) -> Result<bool> {
        if !step.fresh_context
            && member.tags.get("st3.fresh_context").map(String::as_str) != Some("true")
        {
            return Ok(true);
        }
        let operation = crate::model::fresh_context_operation(step);
        let matches_operation = |claim: &crate::model::ClaimRecord| {
            claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("fresh-context")
                && claim
                    .body
                    .pointer("/fields/operation")
                    .and_then(Value::as_str)
                    == Some(operation.as_str())
        };
        if self
            .store
            .observations_for(agent, "runtime.action.succeeded")?
            .iter()
            .any(|claim| matches_operation(claim) && claim_incarnation(claim) == Some(incarnation))
        {
            return Ok(true);
        }
        let request = self
            .store
            .observations_for(agent, "runtime.action.requested")?
            .into_iter()
            .rev()
            .find(&matches_operation);
        if let Some(request) = request {
            if claim_incarnation(&request) != Some(incarnation) {
                self.store.append_claim(&ClaimInput {
                    subject: agent.into(),
                    kind: "runtime.action.succeeded".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), Value::String("fresh-context".into())),
                        ("operation".into(), Value::String(operation.clone())),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                    ]),
                    evidence: vec![request.id],
                    expected_subject: None,
                    idempotency_key: Some(format!("{operation}:ready:{incarnation}")),
                })?;
                self.signal_changed();
                return Ok(true);
            }
        } else {
            self.store.append_claim(&ClaimInput {
                subject: agent.into(),
                kind: "runtime.action.requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("action".into(), Value::String("fresh-context".into())),
                    ("operation".into(), Value::String(operation.clone())),
                    (
                        "runtime_id".into(),
                        Value::String(member.runtime_id.clone()),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{operation}:request")),
            })?;
            self.signal_changed();
        }
        let observation = RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: member.terminal,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some(incarnation.into()),
        };
        self.reconcile_runtime_stop(
            agent,
            &member.runtime_id,
            member.terminal,
            Some(incarnation),
            member.shutdown_timeout_ms,
            Some(&observation),
        )?;
        Ok(false)
    }

    fn reconcile_work_messages(
        &self,
        agent: &str,
        incarnation: &str,
        member: Option<&MemberSpec>,
    ) -> Result<()> {
        let incarnation_key = harness_incarnation_key(incarnation);
        // Closed work wakes still count as attempts. A closed wake can mean the
        // harness started a turn but could not claim this independent step yet;
        // forgetting it would replay attempt 1 and notify this reconciler forever.
        let messages = self.store.work_wake_messages_for_reconcile(agent)?;
        let work = self.store.work_for_reconcile(agent)?;
        let harness = self.store.current_harness(agent)?;

        if !harness.as_ref().is_some_and(CurrentHarnessView::is_ready) {
            return Ok(());
        }

        let mut message_errors = Vec::new();
        for message in messages.iter().filter(|message| message.status != "closed") {
            // One message whose close fails does not keep the agent's other messages open or
            // hold back its next wake.
            let closed = (|| -> Result<()> {
                let Some((step_subject, attempt, readiness_epoch, message_incarnation)) =
                    work_message_target(message)
                else {
                    return Ok(());
                };
                let turn_acknowledged = work
                    .iter()
                    .find(|step| step.subject == step_subject)
                    .is_some_and(|step| {
                        message_sent_at(&self.store, message).is_some_and(|requested| {
                            harness.as_ref().is_some_and(|harness| {
                                step.status == "ready"
                                    && harness.state == "working"
                                    && harness.observed_at_unix_ms >= requested
                            })
                        })
                    });
                if !work_message_should_close(
                    &work,
                    step_subject,
                    attempt,
                    readiness_epoch,
                    message_incarnation,
                    &incarnation_key,
                    turn_acknowledged,
                ) {
                    return Ok(());
                }
                if message.status == "delivered" {
                    self.store.append_claim(&ClaimInput {
                        subject: message.subject.clone(),
                        kind: "message.read".into(),
                        actor: Some(agent.into()),
                        fields: BTreeMap::from([("status".into(), Value::String("read".into()))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("work-message-read:{}", message.subject)),
                    })?;
                }
                if message.status == "sent" && turn_acknowledged {
                    return Ok(());
                }
                self.store.append_claim(&ClaimInput {
                    subject: message.subject.clone(),
                    kind: "message.closed".into(),
                    actor: Some(
                        if matches!(message.status.as_str(), "sent" | "staged") {
                            "daemon/runtime"
                        } else {
                            agent
                        }
                        .into(),
                    ),
                    fields: BTreeMap::from([("status".into(), Value::String("closed".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("message-close:{}", message.subject)),
                })?;
                self.signal_changed();
                Ok(())
            })();
            if let Err(error) = closed {
                message_errors.push(format!("{}: {error:#}", message.subject));
            }
        }

        let run_order = if seat_chooses_between_runs(agent, &work) {
            self.store.seat_run_order(agent)?
        } else {
            Vec::new()
        };
        let next_wake = next_work_wake_for_agent(agent, &work, &run_order);
        for step in work
            .iter()
            .filter(|step| Some(step.subject.as_str()) == next_wake)
        {
            if defers_inherited_work_wake(step, &work, harness.as_ref()) {
                continue;
            }
            if let Some(member) = member
                && !self.prepare_fresh_context(agent, incarnation, step, member)?
            {
                continue;
            }
            let tag_value = format!(
                "{}@{}@{}@{}",
                step.subject, step.attempt, step.readiness_epoch, incarnation_key
            );
            let mut attempts = messages
                .iter()
                .filter(|message| {
                    !message
                        .tags
                        .iter()
                        .any(|tag| tag == "st3-wake-source:manual")
                })
                .filter(|message| {
                    work_message_target(message).is_some_and(
                        |(subject, attempt, readiness_epoch, message_incarnation)| {
                            subject == step.subject
                                && attempt == step.attempt
                                && readiness_epoch == step.readiness_epoch
                                && message_incarnation == incarnation_key
                        },
                    )
                })
                .filter_map(|message| message_sent_at(&self.store, message).map(|at| (at, message)))
                .collect::<Vec<_>>();
            attempts.sort_by_key(|(at, _)| *at);
            let acknowledged = work_wake_acknowledged(&attempts, harness.as_ref());
            if acknowledged {
                continue;
            }
            let attempt_count = u32::try_from(attempts.len()).unwrap_or(u32::MAX);
            match work_wake_decision(
                attempt_count,
                attempts.last().map(|(last, _)| *last),
                attempts.first().map(|(first, _)| *first),
                acknowledged,
                now_ms(),
            ) {
                WorkWakeDecision::Request(wake_attempt) => {
                    append_work_wake_message(
                        &self.store,
                        step,
                        agent,
                        incarnation,
                        wake_attempt,
                        "automatic",
                        "daemon/runtime",
                        "ready assigned work",
                        format!("work-wake:{agent}:{tag_value}:{wake_attempt}"),
                    )?;
                    self.signal_changed();
                }
                WorkWakeDecision::Exhaust => {
                    let reason = format!(
                        "`{agent}` did not start a turn or claim `{}` after {attempt_count} supported driver wake attempts",
                        step.subject
                    );
                    let diagnostic_key =
                        format!("work-wake-exhausted:{agent}:{tag_value}:{attempt_count}");
                    if self.store.operation_claim(&diagnostic_key)?.is_none() {
                        let evidence = work_wake_attempt_evidence(&self.store, &attempts)?;
                        self.store.append_claim(&ClaimInput {
                            subject: agent.into(),
                            kind: "harness.diagnostic".into(),
                            actor: None,
                            fields: BTreeMap::from([
                                ("severity".into(), Value::String("error".into())),
                                ("status".into(), Value::String("failed".into())),
                                ("code".into(), Value::String("work-wake-exhausted".into())),
                                ("reason".into(), Value::String(reason)),
                                ("incarnation_id".into(), Value::String(incarnation.into())),
                                ("step_run".into(), Value::String(step.subject.clone())),
                                ("wake_attempts".into(), Value::from(attempt_count)),
                                ("attempt".into(), Value::from(step.attempt)),
                                ("readiness_epoch".into(), Value::from(step.readiness_epoch)),
                            ]),
                            evidence,
                            expected_subject: None,
                            idempotency_key: Some(diagnostic_key),
                        })?;
                        self.signal_changed();
                    }
                }
                WorkWakeDecision::Wait => {}
            }
        }
        anyhow::ensure!(
            message_errors.is_empty(),
            "close work messages: {}",
            message_errors.join("; ")
        );
        Ok(())
    }

    fn member_was_launched_for_selected_desired(&self, subject: &str) -> Result<bool> {
        let Some(desired_token) = self.store.selected_desired_token(subject)? else {
            return Ok(false);
        };
        Ok(self
            .store
            .observations_for(subject, "runtime.action.succeeded")?
            .into_iter()
            .rev()
            .any(|claim| {
                claim
                    .body
                    .pointer("/fields/desired_token")
                    .and_then(Value::as_str)
                    == Some(desired_token.as_str())
            }))
    }

    pub fn attach(&self, runtime_id: &str) -> Result<()> {
        self.runtime.attach(runtime_id)
    }

    fn reconcile_stop(
        &self,
        subject: &DesiredSubject,
        ptys: Option<&HashMap<String, RuntimeObservation>>,
    ) -> Result<()> {
        let Some(actual) = self.store.latest_actual_value(&subject.subject)? else {
            // A stop-only declaration with no observed runtime is already satisfied.
            // Only its declaring host may project that fact. Otherwise every peer
            // creates a concurrent runtime observation for the same subject.
            if subject.kind == "stop"
                && subject.member.is_none()
                && self
                    .store
                    .selected_desired_origin(&subject.subject)?
                    .as_deref()
                    == Some(self.host.as_str())
            {
                self.record_once(
                    &subject.subject,
                    "runtime.observed",
                    BTreeMap::from([
                        ("status".into(), Value::String("absent".into())),
                        ("reachability".into(), Value::String("reachable".into())),
                    ]),
                )?;
            }
            return Ok(());
        };
        let fields = actual.get("fields").unwrap_or(&actual);
        let selected_origin = self.store.selected_actual_origin(&subject.subject)?;
        let owner_host = subject
            .member
            .as_ref()
            .map(|member| member.host.as_str())
            .or_else(|| fields.get("host").and_then(Value::as_str))
            .or(selected_origin.as_deref());
        if owner_host != Some(self.host.as_str()) {
            // A stop intent is fleet-visible, but only the runtime's owner can
            // observe or terminate its process. A remote empty PTY snapshot is
            // not evidence that the owner's process stopped.
            return Ok(());
        }
        let Some(runtime_id) = fields.get("runtime_id").and_then(Value::as_str) else {
            return Ok(());
        };
        let terminal = fields
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let observation = if terminal {
            // Without a snapshot the PTY may still run, so the stop waits for the next one.
            let Some(ptys) = ptys else {
                return Ok(());
            };
            ptys.get(runtime_id).cloned()
        } else {
            self.runtime.observe_exec(runtime_id)?
        };
        // A stopped projection is only a record of the last observation. The same
        // runtime ID can be live again (or a stop can have raced with a restart),
        // so fence the observed process before allowing run cleanup to finish.
        if let Some(observation) = observation.as_ref().filter(|item| item.status == "running") {
            let mut observed_fields = BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("runtime_id".into(), Value::String(runtime_id.into())),
                ("terminal".into(), Value::Bool(terminal)),
                ("reachability".into(), Value::String("reachable".into())),
            ]);
            if let Some(incarnation) = &observation.incarnation_id {
                observed_fields.insert("incarnation_id".into(), Value::String(incarnation.clone()));
            }
            if let Some(timeout) = fields.get("shutdown_timeout_ms") {
                observed_fields.insert("shutdown_timeout_ms".into(), timeout.clone());
            }
            self.record_once(&subject.subject, "runtime.observed", observed_fields)?;
        }
        let incarnation = observation
            .as_ref()
            .and_then(|value| value.incarnation_id.as_deref())
            .or_else(|| fields.get("incarnation_id").and_then(Value::as_str));
        let timeout = fields
            .get("shutdown_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(5_000);
        self.reconcile_runtime_stop(
            &subject.subject,
            runtime_id,
            terminal,
            incarnation,
            timeout,
            observation.as_ref(),
        )?;
        Ok(())
    }

    /// Create a declared checkout before its agent starts. Returns why the workspace is still
    /// unavailable. After a failure, Git runs again only after `CHECKOUT_RETRY_MS`.
    fn create_checkout(
        &self,
        subject: &str,
        checkout: &Checkout,
        workspace: &Path,
    ) -> Result<Option<String>> {
        let now = now_ms();
        if let Some((_, failure)) = self
            .checkout_retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(subject)
            .filter(|(retry_at, _)| *retry_at > now)
        {
            return Ok(Some(failure.clone()));
        }
        match checkout.create(workspace) {
            Ok(warnings) => {
                self.checkout_retries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(subject);
                for warning in warnings {
                    self.record_once(
                        subject,
                        "harness.diagnostic",
                        BTreeMap::from([
                            ("severity".into(), Value::String("warning".into())),
                            ("status".into(), Value::String("warning".into())),
                            ("code".into(), Value::String("checkout-fetch-failed".into())),
                            ("reason".into(), Value::String(warning)),
                        ]),
                    )?;
                }
                Ok(None)
            }
            Err(error) => {
                let failure = format!(
                    "checkout of {} failed: {error:#}",
                    checkout.repository.display()
                );
                let retry_at = now.saturating_add(CHECKOUT_RETRY_MS);
                self.checkout_retries
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(subject.into(), (retry_at, failure.clone()));
                self.arm_restart(&format!("checkout:{subject}"), retry_at);
                Ok(Some(failure))
            }
        }
    }

    /// Remove a finished run's checkout once its runtime has stopped, when the agent asked for
    /// `remove-at-run-end`.
    fn remove_checkout_after_run(
        &self,
        subject: &DesiredSubject,
        live_workspaces: &BTreeSet<&str>,
    ) -> Result<()> {
        let Some((checkout, workspace)) = self.finished_run_checkout(subject)? else {
            return Ok(());
        };
        self.remove_finished_checkout(
            &subject.subject,
            &checkout,
            Path::new(&workspace),
            live_workspaces,
        )
    }

    /// The checkout and workspace of a stopped agent whose owning run has ended. A run's cleanup
    /// replaces the agent's declaration with a stop, so the checkout comes from the last agent
    /// declaration.
    fn finished_run_checkout(
        &self,
        subject: &DesiredSubject,
    ) -> Result<Option<(Checkout, String)>> {
        let Some(run) = subject.owner_run.as_deref() else {
            return Ok(None);
        };
        let Some((checkout, workspace)) = self.declared_run_end_checkout(&subject.subject)? else {
            return Ok(None);
        };
        // A run stops its owned agents in its cleanup phase, before it becomes terminal.
        let run_ended = self.store.mission_run(run)?.is_some_and(|run| {
            matches!(run.status.as_str(), "completed" | "failed" | "cancelled")
                || run.phase == "terminal"
                || run.phase.starts_with("cleanup-")
        });
        let stopped = self
            .store
            .latest_actual_value(&subject.subject)?
            .is_some_and(|actual| {
                actual_field(&actual, "status").and_then(Value::as_str) == Some("stopped")
            });
        Ok((run_ended && stopped).then_some((checkout, workspace)))
    }

    /// The run-end checkout and workspace of the subject's last agent declaration for this host.
    /// Every stop subject asks on every pass. Declarations are append-only claims, so the parsed
    /// answer holds until another `intent.desired` claim arrives for the subject.
    fn declared_run_end_checkout(&self, subject: &str) -> Result<Option<(Checkout, String)>> {
        let newest = self.store.newest_claim_index(subject, "intent.desired")?;
        if let Some((index, declared)) = self
            .declared_checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(subject)
            && *index == newest
        {
            return Ok(declared.clone());
        }
        #[cfg(test)]
        DECLARATION_PARSES.with(|parses| parses.set(parses.get() + 1));
        let declared = self
            .store
            .claims_for(subject, Some("intent.desired"))?
            .into_iter()
            .rev()
            .find(|claim| claim.body.get("kind").and_then(Value::as_str) == Some("agent"))
            .and_then(|declaration| {
                let checkout = declaration
                    .body
                    .get("desired")
                    .and_then(Checkout::from_desired)
                    .filter(|checkout| checkout.remove_at_run_end)?;
                let member = declaration
                    .body
                    .get("member")
                    .and_then(|member| serde_json::from_value::<MemberSpec>(member.clone()).ok())
                    .filter(|member| member.host == self.host)?;
                Some((checkout, member.workspace))
            });
        let mut declared_checkouts = self
            .declared_checkouts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if declared_checkouts.len() >= DECLARED_CHECKOUT_LIMIT
            && !declared_checkouts.contains_key(subject)
        {
            declared_checkouts.clear();
        }
        declared_checkouts.insert(subject.to_owned(), (newest, declared.clone()));
        Ok(declared)
    }

    /// Remove a finished checkout unless a current member uses its workspace. A worktree with
    /// changes stays, with one warning, and is tried again after `CHECKOUT_RETRY_MS`.
    fn remove_finished_checkout(
        &self,
        subject: &str,
        checkout: &Checkout,
        workspace: &Path,
        live_workspaces: &BTreeSet<&str>,
    ) -> Result<()> {
        if live_workspaces.contains(workspace.to_string_lossy().as_ref()) || !workspace.exists() {
            return Ok(());
        }
        let now = now_ms();
        if self
            .checkout_retries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(subject)
            .is_some_and(|(retry_at, _)| *retry_at > now)
        {
            return Ok(());
        }
        if let Err(error) = checkout.remove(workspace) {
            let reason = format!("kept worktree {}: {error:#}", workspace.display());
            self.checkout_retries
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    subject.into(),
                    (now.saturating_add(CHECKOUT_RETRY_MS), reason.clone()),
                );
            self.record_once(
                subject,
                "harness.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("status".into(), Value::String("warning".into())),
                    ("code".into(), Value::String("checkout-kept".into())),
                    ("reason".into(), Value::String(reason)),
                ]),
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn reconcile_runtime_stop(
        &self,
        subject: &str,
        runtime_id: &str,
        terminal: bool,
        incarnation: Option<&str>,
        timeout_ms: u64,
        observation: Option<&RuntimeObservation>,
    ) -> Result<bool> {
        // A runtime whose state could not be read may still run. Wait for a readable observation.
        if observation.is_some_and(|observation| observation.status == "unknown") {
            return Ok(false);
        }
        if observation.is_none_or(|observation| observation.status != "running") {
            self.record_once(
                subject,
                "runtime.observed",
                BTreeMap::from([
                    ("status".into(), Value::String("stopped".into())),
                    ("runtime_id".into(), Value::String(runtime_id.into())),
                    ("terminal".into(), Value::Bool(terminal)),
                    ("reachability".into(), Value::String("reachable".into())),
                    ("reason".into(), Value::Null),
                ]),
            )?;
            return Ok(true);
        }
        let incarnation = incarnation.unwrap_or("unknown");
        let requests = self
            .store
            .observations_for(subject, "runtime.action.requested")?;
        let request = requests.into_iter().rev().find(|claim| {
            claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("terminate")
                && claim
                    .body
                    .pointer("/fields/incarnation_id")
                    .and_then(Value::as_str)
                    == Some(incarnation)
        });
        let Some(request) = request else {
            let deadline = now_ms().saturating_add(timeout_ms as u128);
            let request = self.store.append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.action.requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("action".into(), Value::String("terminate".into())),
                    ("runtime_id".into(), Value::String(runtime_id.into())),
                    ("terminal".into(), Value::Bool(terminal)),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    (
                        "deadline_unix_ms".into(),
                        Value::String(deadline.to_string()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("terminate:{subject}:{incarnation}")),
            })?;
            if let Err(error) = self.runtime.stop(runtime_id, terminal, Some(incarnation)) {
                self.store.append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.action.failed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("action".into(), Value::String("terminate".into())),
                        ("reason".into(), Value::String(error.to_string())),
                    ]),
                    evidence: vec![request.id],
                    expected_subject: None,
                    idempotency_key: Some(format!("terminate-failed:{subject}:{incarnation}")),
                })?;
                return Err(error);
            }
            self.arm_restart(&format!("stop:{subject}"), now_ms().saturating_add(100));
            return Ok(false);
        };
        let deadline = request
            .body
            .pointer("/fields/deadline_unix_ms")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(
                request
                    .accepted_at_unix_ms
                    .saturating_add(timeout_ms as u128),
            );
        if now_ms() < deadline {
            self.arm_restart(&format!("stop:{subject}"), deadline);
            return Ok(false);
        }
        let deadline_key = format!("stop-deadline:{subject}:{incarnation}");
        let deadline_record = self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.deadline-reached".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String("terminate".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: vec![request.id],
            expected_subject: None,
            idempotency_key: Some(deadline_key.clone()),
        })?;
        if self
            .store
            .observations_for(subject, "runtime.action.succeeded")?
            .iter()
            .any(|claim| {
                claim
                    .body
                    .pointer("/fields/deadline_key")
                    .and_then(Value::as_str)
                    == Some(deadline_key.as_str())
            })
        {
            self.record_once(
                subject,
                "runtime.reconcile-decision",
                BTreeMap::from([
                    ("decision".into(), Value::String("raise".into())),
                    ("reachability".into(), Value::String("unreachable".into())),
                    (
                        "reason".into(),
                        Value::String("the recorded incarnation survived SIGKILL".into()),
                    ),
                ]),
            )?;
            return Ok(false);
        }
        self.runtime.kill(runtime_id, terminal, Some(incarnation))?;
        self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String("kill".into())),
                ("deadline_key".into(), Value::String(deadline_key)),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: vec![deadline_record.id],
            expected_subject: None,
            idempotency_key: Some(format!("kill:{subject}:{incarnation}")),
        })?;
        self.arm_restart(&format!("stop:{subject}"), now_ms().saturating_add(100));
        Ok(false)
    }

    fn perform_start(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        reason: &str,
    ) -> Result<()> {
        // A member whose start keeps failing waits between attempts and then parks with one
        // attention request, instead of spawning again on every pass. A gate runner fails its gate.
        if matches!(subject.kind.as_str(), "agent" | "exec" | "pty")
            && member.driver.as_deref() != Some("codex")
            && self.defer_or_park_failed_start(subject)?
        {
            return Ok(());
        }
        if member.driver.as_deref() == Some("codex") {
            let token = self
                .store
                .selected_desired_token(&subject.subject)?
                .unwrap_or_default();
            if self.codex_crash_loop_raised(&subject.subject, &token)? {
                self.raise_codex_crash_loop(
                    &subject.subject,
                    &token,
                    "the prior Codex crash loop remains stopped",
                )?;
                return Ok(());
            }
            let recent_failures = self
                .store
                .observations_for(&subject.subject, "runtime.action.failed")?
                .into_iter()
                .filter(|claim| {
                    claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("start")
                        && claim
                            .body
                            .pointer("/fields/desired_token")
                            .and_then(Value::as_str)
                            == Some(token.as_str())
                        && now_ms().saturating_sub(claim.accepted_at_unix_ms)
                            < CODEX_CRASH_LOOP_INTERVAL_MS
                })
                .collect::<Vec<_>>();
            if recent_failures.len() >= CODEX_CRASH_LOOP_ATTEMPTS {
                let detail = recent_failures
                    .last()
                    .and_then(|claim| claim.body.pointer("/fields/reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("the runtime start failed");
                self.raise_codex_crash_loop(
                    &subject.subject,
                    &token,
                    &format!("Codex failed to launch three times: {detail}"),
                )?;
                return Ok(());
            }
        }
        let workspace = Path::new(&member.workspace);
        if workspace.exists() {
            anyhow::ensure!(
                workspace.is_dir(),
                "workspace {} is not a directory",
                workspace.display()
            );
        } else if member.workspace_create {
            std::fs::create_dir_all(workspace)
                .with_context(|| format!("create workspace {}", workspace.display()))?;
        } else {
            anyhow::bail!(
                "workspace {} does not exist; add create=#true to its workspace declaration to create it",
                workspace.display()
            );
        }
        let mut launch_member = member.clone();
        for (key, value) in &self.runtime_environment {
            launch_member
                .environment
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        launch_member
            .environment
            .insert("ST3_ENDPOINT".into(), self.endpoint.clone());
        launch_member.environment.insert(
            "ST3_DRIVER_STATE_DIR".into(),
            self.driver_state_dir.to_string_lossy().into_owned(),
        );
        launch_member
            .environment
            .insert("ST3_SUBJECT".into(), subject.subject.clone());
        if subject.kind == "agent" {
            launch_member
                .environment
                .insert("ST_AGENT".into(), subject.subject.clone());
        } else if let Some(owner) = member.tags.get("st3.agent") {
            launch_member
                .environment
                .insert("ST_AGENT".into(), owner.clone());
        } else {
            launch_member.environment.remove("ST_AGENT");
        }
        let executable = launch_executable()?;
        launch_member
            .environment
            .insert("ST3_BIN".into(), executable.to_string_lossy().into_owned());
        if let crate::model::LaunchSpec::Argv(argv) = &mut launch_member.launch
            && argv.first().map(String::as_str) == Some("st3")
        {
            argv[0] = executable.to_string_lossy().into_owned();
        }
        if !launch_member.terminal {
            let original = match &launch_member.launch {
                crate::model::LaunchSpec::Shell(source) => {
                    vec!["sh".into(), "-c".into(), source.clone()]
                }
                crate::model::LaunchSpec::Argv(argv) => argv.clone(),
            };
            let mut wrapper = vec![
                executable.to_string_lossy().into_owned(),
                "driver".into(),
                "exec".into(),
                "--subject".into(),
                subject.subject.clone(),
                "--".into(),
            ];
            wrapper.extend(original);
            launch_member.launch = crate::model::LaunchSpec::Argv(wrapper);
        }
        let operation = format!("{}:start", subject.subject);
        self.record_once(
            &subject.subject,
            "runtime.action.requested",
            BTreeMap::from([
                ("action".into(), Value::String("start".into())),
                ("operation".into(), Value::String(operation.clone())),
            ]),
        )?;
        if let Err(error) = self.runtime.start(&launch_member) {
            let reason = error.to_string();
            let desired_token = self
                .store
                .selected_desired_token(&subject.subject)?
                .unwrap_or_default();
            let prior_failures = self.start_failures(&subject.subject, &desired_token)?;
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "runtime.action.failed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("action".into(), Value::String("start".into())),
                    ("operation".into(), Value::String(operation)),
                    ("reason".into(), Value::String(reason)),
                    ("desired_token".into(), Value::String(desired_token.clone())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "start-failed:{}:{desired_token}:{}",
                    subject.subject,
                    prior_failures.len() + 1
                )),
            })?;
            self.record_once(
                &subject.subject,
                "runtime.observed",
                member_fields(member, "absent", None, false),
            )?;
            return Err(error).context("start member runtime");
        }
        let desired_token = self
            .store
            .selected_desired_token(&subject.subject)?
            .unwrap_or_default();
        self.store.append_claim(&ClaimInput {
            subject: subject.subject.clone(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([
                ("desired_token".into(), Value::String(desired_token)),
                (
                    "runtime_id".into(),
                    Value::String(member.runtime_id.clone()),
                ),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        self.record_once(
            &subject.subject,
            "runtime.observed",
            member_fields(member, "starting", None, false),
        )?;
        self.record_once(
            &subject.subject,
            "runtime.action.succeeded",
            BTreeMap::from([
                ("reason".into(), Value::String(reason.into())),
                (
                    "runtime_id".into(),
                    Value::String(member.runtime_id.clone()),
                ),
            ]),
        )?;
        self.signal_changed();
        Ok(())
    }

    fn reconcile_restart(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: &RuntimeObservation,
    ) -> Result<()> {
        if subject.kind == "agent"
            && member.lifecycle == MemberLifecycle::Service
            && member.driver.as_deref() != Some("codex")
            && !self.claude_trust_recovery_stopped(&subject.subject, observation)?
            && self.park_unready_crash_loop(subject)?
        {
            return Ok(());
        }
        if member.driver.as_deref() == Some("codex") {
            let token = self
                .store
                .selected_desired_token(&subject.subject)?
                .unwrap_or_default();
            if self.codex_crash_loop_raised(&subject.subject, &token)? {
                self.raise_codex_crash_loop(
                    &subject.subject,
                    &token,
                    "the prior Codex crash loop remains stopped",
                )?;
                return Ok(());
            }
        }
        match self.restart_decision(subject, member, observation)? {
            RestartDecision::Start => {
                self.delayed_restarts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&subject.subject);
                self.perform_start(subject, member, "the prior generation exited")
            }
            RestartDecision::Wait { until, reason } => {
                self.record_once(
                    &subject.subject,
                    "runtime.reconcile-decision",
                    BTreeMap::from([
                        ("decision".into(), Value::String("wait".into())),
                        ("reachability".into(), Value::String("reachable".into())),
                        ("reason".into(), Value::String(reason)),
                        (
                            "restart_at_unix_ms".into(),
                            Value::String(until.to_string()),
                        ),
                    ]),
                )?;
                self.arm_restart(&subject.subject, until);
                Ok(())
            }
            RestartDecision::Fail { reason } => {
                if member.driver.as_deref() == Some("codex") {
                    let token = self
                        .store
                        .selected_desired_token(&subject.subject)?
                        .unwrap_or_default();
                    let diagnostic = self
                        .store
                        .claims_for(&subject.subject, Some("harness.diagnostic"))?
                        .into_iter()
                        .rev()
                        .find(|claim| {
                            claim.body.pointer("/fields/code").and_then(Value::as_str)
                                == Some("codex-driver-failed")
                                && claim
                                    .body
                                    .pointer("/fields/incarnation_id")
                                    .and_then(Value::as_str)
                                    == observation.incarnation_id.as_deref()
                        })
                        .and_then(|claim| {
                            claim
                                .body
                                .pointer("/fields/reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        });
                    let detail = diagnostic
                        .or_else(|| {
                            observation
                                .exit_code
                                .map(|code| format!("last exit code {code}"))
                        })
                        .map_or(reason.clone(), |detail| format!("{reason}; {detail}"));
                    self.raise_codex_crash_loop(&subject.subject, &token, &detail)
                } else {
                    self.record_once(
                        &subject.subject,
                        "runtime.reconcile-decision",
                        BTreeMap::from([
                            ("decision".into(), Value::String("raise".into())),
                            ("reachability".into(), Value::String("unreachable".into())),
                            ("reason".into(), Value::String(reason)),
                        ]),
                    )
                }
            }
        }
    }

    fn codex_crash_loop_raised(&self, subject: &str, token: &str) -> Result<bool> {
        let decision_key = format!("codex-crash-loop:{token}");
        Ok(self
            .store
            .claims_for(subject, Some("runtime.reconcile-decision"))?
            .iter()
            .any(|claim| {
                claim.body.pointer("/fields/key").and_then(Value::as_str)
                    == Some(decision_key.as_str())
            }))
    }

    fn start_failures(&self, subject: &str, token: &str) -> Result<Vec<crate::model::ClaimRecord>> {
        Ok(self
            .store
            .observations_for(subject, "runtime.action.failed")?
            .into_iter()
            .filter(|claim| {
                claim.body.pointer("/fields/action").and_then(Value::as_str) == Some("start")
                    && claim
                        .body
                        .pointer("/fields/desired_token")
                        .and_then(Value::as_str)
                        == Some(token)
            })
            .collect())
    }

    /// Keep a failed durable launch from turning each graph write into another PTY spawn.
    fn defer_or_park_failed_start(&self, subject: &DesiredSubject) -> Result<bool> {
        let token = self
            .store
            .selected_desired_token(&subject.subject)?
            .unwrap_or_default();
        if self.runtime_crash_loop_raised(&subject.subject, &token)? {
            return Ok(true);
        }
        let last_success_index = self
            .store
            .observations_for(&subject.subject, "runtime.action.succeeded")?
            .into_iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/desired_token")
                    .and_then(Value::as_str)
                    == Some(token.as_str())
            })
            .map(|claim| claim.store_index)
            .max()
            .unwrap_or(0);
        let now = now_ms();
        let recent = self
            .start_failures(&subject.subject, &token)?
            .into_iter()
            .filter(|claim| {
                claim.store_index > last_success_index
                    && now.saturating_sub(claim.accepted_at_unix_ms) < CODEX_CRASH_LOOP_INTERVAL_MS
            })
            .collect::<Vec<_>>();
        if recent.len() >= CODEX_CRASH_LOOP_ATTEMPTS {
            let detail = recent
                .last()
                .and_then(|claim| claim.body.pointer("/fields/reason"))
                .and_then(Value::as_str)
                .unwrap_or("the runtime start failed");
            self.raise_runtime_crash_loop(
                &subject.subject,
                &token,
                &format!("the runtime failed to start three times: {detail}"),
            )?;
            return Ok(true);
        }
        if let Some(last) = recent.last() {
            let until = last.accepted_at_unix_ms.saturating_add(15_000);
            if until > now {
                self.arm_restart(&subject.subject, until);
                return Ok(true);
            }
        }
        self.delayed_restarts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&subject.subject);
        Ok(false)
    }

    fn park_unready_crash_loop(&self, subject: &DesiredSubject) -> Result<bool> {
        let token = self
            .store
            .selected_desired_token(&subject.subject)?
            .unwrap_or_default();
        if self.runtime_crash_loop_raised(&subject.subject, &token)? {
            return Ok(true);
        }
        let first_launch = self
            .store
            .observations_for(&subject.subject, "runtime.action.succeeded")?
            .into_iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/desired_token")
                    .and_then(Value::as_str)
                    == Some(token.as_str())
            })
            .map(|claim| claim.accepted_at_unix_ms)
            .min();
        let Some(first_launch) = first_launch else {
            return Ok(false);
        };
        let ready = self
            .store
            .claims_for(&subject.subject, Some("harness.observed"))?
            .into_iter()
            .filter(|claim| {
                matches!(
                    claim.body.pointer("/fields/state").and_then(Value::as_str),
                    Some("ready" | "idle" | "working")
                )
            })
            .filter_map(|claim| claim_incarnation(&claim).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let trust_recovery = self
            .claude_trust_prompts(&subject.subject)?
            .iter()
            .filter_map(|claim| claim_incarnation(claim).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let now = now_ms();
        let exits = self
            .store
            .claims_for(&subject.subject, Some("runtime.observed"))?
            .into_iter()
            .filter(|claim| {
                matches!(
                    claim.body.pointer("/fields/status").and_then(Value::as_str),
                    Some("exited" | "vanished")
                ) && claim.accepted_at_unix_ms >= first_launch
                    && now.saturating_sub(claim.accepted_at_unix_ms) < CODEX_CRASH_LOOP_INTERVAL_MS
            })
            .filter_map(|claim| claim_incarnation(&claim).map(str::to_owned))
            .filter(|incarnation| {
                !ready.contains(incarnation) && !trust_recovery.contains(incarnation)
            })
            .collect::<BTreeSet<_>>();
        if exits.len() >= CODEX_CRASH_LOOP_ATTEMPTS {
            self.raise_runtime_crash_loop(
                &subject.subject,
                &token,
                "the runtime exited three times before the harness became ready",
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    fn runtime_crash_loop_raised(&self, subject: &str, token: &str) -> Result<bool> {
        let key = format!("runtime-crash-loop:{token}");
        Ok(self
            .store
            .claims_for(subject, Some("runtime.reconcile-decision"))?
            .iter()
            .any(|claim| {
                claim.body.pointer("/fields/key").and_then(Value::as_str) == Some(key.as_str())
            }))
    }

    fn raise_runtime_crash_loop(&self, subject: &str, token: &str, reason: &str) -> Result<()> {
        let key = format!("runtime-crash-loop:{subject}:{token}");
        let decision_key = format!("runtime-crash-loop:{token}");
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention_subject = format!("attention/{}", &digest[..32]);
        let mut changed = false;
        if !self.runtime_crash_loop_raised(subject, token)? {
            self.store.append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.reconcile-decision".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("decision".into(), Value::String("raise".into())),
                    ("reachability".into(), Value::String("unreachable".into())),
                    ("key".into(), Value::String(decision_key)),
                    ("reason".into(), Value::String(reason.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{key}:raised")),
            })?;
            changed = true;
        }
        if self.store.attention_request(&attention_subject)?.is_none() {
            self.store.request_attention(&attention_subject, &AttentionRequest {
                reviewer: "person/operator".into(),
                title: "An agent stopped after repeated runtime failures".into(),
                reason: format!("{subject}: {reason}. Inspect the seat and revise its desired declaration before restarting."),
                severity: "error".into(), targets: vec![subject.into()],
                actor: "agent/st3/reconciler".into(),
                idempotency_key: format!("{key}:attention"),
            })?;
            changed = true;
        }
        if changed {
            self.signal_changed();
        }
        Ok(())
    }

    fn raise_codex_crash_loop(&self, subject: &str, token: &str, reason: &str) -> Result<()> {
        let key = format!("codex-crash-loop:{subject}:{token}");
        let decision_key = format!("codex-crash-loop:{token}");
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention_subject = format!("attention/{}", &digest[..32]);
        let existing = self
            .store
            .claims_for(subject, Some("runtime.reconcile-decision"))?
            .into_iter()
            .find(|claim| {
                claim.body.pointer("/fields/key").and_then(Value::as_str)
                    == Some(decision_key.as_str())
            });
        let retained_reason = existing
            .as_ref()
            .and_then(|claim| claim.body.pointer("/fields/reason"))
            .and_then(Value::as_str)
            .unwrap_or(reason);
        let mut changed = false;
        if existing.is_none() {
            self.store.append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.reconcile-decision".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("decision".into(), Value::String("raise".into())),
                    ("reachability".into(), Value::String("unreachable".into())),
                    ("key".into(), Value::String(decision_key.clone())),
                    ("reason".into(), Value::String(reason.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{key}:raised")),
            })?;
            changed = true;
        }
        if self.store.attention_request(&attention_subject)?.is_none() {
            self.store.request_attention(
                &attention_subject,
                &AttentionRequest {
                    reviewer: "person/nathan".into(),
                    title: "A Codex agent stopped after repeated failures".into(),
                    reason: format!("{subject}: {retained_reason}. Inspect the seat and revise its desired declaration before restarting."),
                    severity: "error".into(),
                    targets: vec![subject.into()],
                    actor: "agent/st3/reconciler".into(),
                    idempotency_key: format!("{key}:attention"),
                },
            )?;
            changed = true;
        }
        if changed {
            self.signal_changed();
        }
        Ok(())
    }

    fn restart_decision(
        &self,
        subject: &DesiredSubject,
        member: &MemberSpec,
        observation: &RuntimeObservation,
    ) -> Result<RestartDecision> {
        let intensity = if member.driver.as_deref() == Some("codex") {
            RestartIntensity {
                attempts: CODEX_CRASH_LOOP_ATTEMPTS as u32,
                interval_ms: CODEX_CRASH_LOOP_INTERVAL_MS as u64,
                delay_ms: member.restart_intensity.delay_ms,
                mode: "fail".into(),
            }
        } else {
            member.restart_intensity.clone()
        };
        let now = now_ms();
        let desired_token = self
            .store
            .selected_desired_token(&subject.subject)?
            .unwrap_or_default();
        let mut launches = self
            .store
            .observations_for(&subject.subject, "runtime.action.succeeded")?
            .into_iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/desired_token")
                    .and_then(Value::as_str)
                    == Some(desired_token.as_str())
            })
            .collect::<Vec<_>>();
        let resets = self
            .store
            .claims_for(&subject.subject, Some("runtime.restart-window-reset"))?;
        let incarnation = observation.incarnation_id.as_deref().unwrap_or("unknown");
        let reset_index = resets
            .iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/desired_token")
                    .and_then(Value::as_str)
                    == Some(desired_token.as_str())
                    && claim
                        .body
                        .pointer("/fields/incarnation_id")
                        .and_then(Value::as_str)
                        == Some(incarnation)
            })
            .map(|claim| claim.store_index)
            .max()
            .unwrap_or(0);
        launches.retain(|claim| {
            launch_in_restart_window(
                claim.store_index,
                claim.accepted_at_unix_ms,
                reset_index,
                now,
                intensity.interval_ms,
                &intensity.mode,
            )
        });

        if intensity.mode == "fail" {
            if let Some(last) = launches.last()
                && now.saturating_sub(last.accepted_at_unix_ms) >= intensity.interval_ms as u128
            {
                self.store.append_claim(&ClaimInput {
                    subject: subject.subject.clone(),
                    kind: "runtime.restart-window-reset".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("desired_token".into(), Value::String(desired_token.clone())),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                        (
                            "reason".into(),
                            Value::String("the stable interval cleared the restart window".into()),
                        ),
                    ]),
                    evidence: vec![last.id.clone()],
                    expected_subject: None,
                    idempotency_key: Some(format!(
                        "restart-reset:{}:{desired_token}:{incarnation}",
                        subject.subject
                    )),
                })?;
                launches.clear();
            }
            if launches.len() >= intensity.attempts as usize {
                return Ok(RestartDecision::Fail {
                    reason: format!(
                        "the member used {} launches without a stable {}ms interval",
                        intensity.attempts, intensity.interval_ms
                    ),
                });
            }
        }

        let mut wait_until = now;
        let mut reasons = Vec::new();
        if intensity.delay_ms > 0 {
            let observed_at = self
                .store
                .claims_for(&subject.subject, Some("runtime.observed"))?
                .into_iter()
                .rev()
                .find(|claim| {
                    matches!(
                        claim.body.pointer("/fields/status").and_then(Value::as_str),
                        Some("exited" | "vanished")
                    ) && observation
                        .incarnation_id
                        .as_deref()
                        .is_none_or(|incarnation| {
                            claim
                                .body
                                .pointer("/fields/incarnation_id")
                                .and_then(Value::as_str)
                                == Some(incarnation)
                        })
                })
                .map(|claim| claim.accepted_at_unix_ms)
                .unwrap_or(now);
            let delayed = observed_at.saturating_add(intensity.delay_ms as u128);
            if delayed > wait_until {
                wait_until = delayed;
                reasons.push(format!("the restart delay is {}ms", intensity.delay_ms));
            }
        }
        if intensity.mode == "delay" {
            let window_start = now.saturating_sub(intensity.interval_ms as u128);
            let recent = launches
                .iter()
                .filter(|claim| claim.accepted_at_unix_ms > window_start)
                .collect::<Vec<_>>();
            if recent.len() >= intensity.attempts as usize {
                let available = recent[0]
                    .accepted_at_unix_ms
                    .saturating_add(intensity.interval_ms as u128);
                if available > wait_until {
                    wait_until = available;
                    reasons.push(format!(
                        "the {} launch limit applies for {}ms",
                        intensity.attempts, intensity.interval_ms
                    ));
                }
            }
        }
        if wait_until > now {
            Ok(RestartDecision::Wait {
                until: wait_until,
                reason: reasons.join("; "),
            })
        } else {
            Ok(RestartDecision::Start)
        }
    }

    fn arm_restart(&self, subject: &str, until: u128) {
        let mut armed = self
            .delayed_restarts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if armed.get(subject).is_some_and(|current| *current == until) {
            return;
        }
        armed.insert(subject.into(), until);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let notify = self.notify.clone();
        let delayed = self.delayed_restarts.clone();
        let subject = subject.to_owned();
        handle.spawn(async move {
            let delay = until.saturating_sub(now_ms()).min(u64::MAX as u128) as u64;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            delayed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&subject);
            notify.notify_one();
        });
    }

    fn record_member(
        &self,
        subject: &DesiredSubject,
        observation: &RuntimeObservation,
        adopted: bool,
    ) -> Result<()> {
        let member = subject
            .member
            .as_ref()
            .context("member observation lacks member")?;
        let mut fields = member_fields(
            member,
            &observation.status,
            observation.incarnation_id.as_deref(),
            adopted,
        );
        if let Some(exit_code) = observation.exit_code {
            fields.insert("exit_code".into(), Value::from(exit_code));
        }
        self.record_once(&subject.subject, "runtime.observed", fields)
    }

    fn record_once(
        &self,
        subject: &str,
        kind: &str,
        fields: BTreeMap<String, Value>,
    ) -> Result<()> {
        if self
            .store
            .latest_observation(subject, kind)?
            .is_some_and(|claim| {
                claim.body.get("fields")
                    == Some(&serde_json::to_value(&fields).unwrap_or(Value::Null))
            })
        {
            return Ok(());
        }
        let record = self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        if crate::store::local_observation_position(&record).is_some() {
            // Only this node reads a local observation, and it cannot advance a mission.
            self.event_notify
                .send_modify(|generation| *generation = generation.saturating_add(1));
        } else {
            self.signal_changed();
        }
        Ok(())
    }

    /// Evaluate each active run on its own. A run that fails records a fault on that run, and
    /// every other run, including runs in cleanup, is still evaluated in the same pass.
    fn evaluate_mission_runs(&self) -> Result<()> {
        let ids = self.store.active_mission_run_ids_for_origin(&self.host)?;
        let mut active_generations = BTreeSet::new();
        let mut active_steps = BTreeSet::new();
        let mut changed = false;
        for id in &ids {
            let subject = format!("mission-run/{id}");
            changed |= self
                .isolate("mission-run", &subject, || {
                    let run = self.store.mission_run_for_reconcile(id)?;
                    active_generations.insert(run.generation.clone());
                    active_steps.extend(run.steps.iter().map(|step| step.subject.clone()));
                    self.evaluate_active_mission_run(&run)
                })
                .unwrap_or(false);
        }
        self.materialized_mission_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|generation| active_generations.contains(generation.as_str()));
        self.retired_predecessor_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|generation| active_generations.contains(generation.as_str()));
        // A run or step that left the active set while faulted has nothing left to fail.
        let active = ids
            .iter()
            .map(|id| format!("mission-run/{id}"))
            .collect::<BTreeSet<_>>();
        let inactive = self
            .open_faults()?
            .iter()
            .flatten()
            .filter(|((subject, scope), _)| match scope.as_str() {
                "mission-run" => !active.contains(subject),
                "step" => !active_steps.contains(subject),
                _ => false,
            })
            .map(|((subject, scope), _)| (subject.clone(), scope.clone()))
            .collect::<Vec<_>>();
        for (subject, scope) in inactive {
            self.close_fault(&subject, &scope, "it is no longer active")?;
        }
        if changed {
            self.signal_changed();
        }
        Ok(())
    }

    fn evaluate_active_mission_run(&self, run: &MissionRunView) -> Result<bool> {
        if run
            .deadline_at_unix_ms
            .is_some_and(|deadline| deadline <= now_ms())
            && !run.phase.starts_with("cleanup-")
        {
            let timeout = run.timeout_ms.unwrap_or_default();
            let reason = format!("the mission timeout expired after {timeout}ms");
            let mut changed = self
                .store
                .terminate_mission_run_descendants(&run.id, &reason)?;
            changed |= self.store.set_mission_run_state(
                &run.id,
                "running",
                "cleanup-failed",
                Some(&reason),
            )?;
            return Ok(changed);
        }
        if run.phase == "revision-draining" {
            if self.store.apply_drained_revision(&run.id)?.is_some() {
                return Ok(true);
            }
            // A replica can receive the old proposal's draining claim after the
            // successor generation. Recover the persisted run phase when no
            // draining proposal still targets its current generation.
            if !self
                .store
                .revision_proposal_for_run(&run.id)?
                .is_some_and(|proposal| proposal.status == "draining")
            {
                return self
                    .store
                    .set_mission_run_state(&run.id, &run.status, "normal", None);
            }
        }
        // Cleanup reads only the run's owned declarations. It never waits for the mission
        // revision or its steps, so a run whose revision is unavailable still stops its runtimes.
        if run.phase.starts_with("cleanup-") {
            return self.reconcile_mission_run_cleanup(run);
        }
        let mission_id = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
        let Some(mission) = self.store.mission_spec(mission_id, Some(&run.revision))? else {
            return self.store.set_mission_run_state(
                &run.id,
                "blocked",
                &run.phase,
                Some("the selected mission revision is unavailable"),
            );
        };
        let mission = match crate::mission::run_mission(mission, run.after.as_deref()) {
            Ok(mission) => mission,
            Err(error) => {
                return self.store.set_mission_run_state(
                    &run.id,
                    "blocked",
                    &run.phase,
                    Some(&error.message),
                );
            }
        };
        self.evaluate_mission_run(run, &mission)
    }

    fn evaluate_mission_run(&self, run: &MissionRunView, mission: &MissionSpec) -> Result<bool> {
        if run.phase.starts_with("cleanup-") {
            return self.reconcile_mission_run_cleanup(run);
        }
        let mut changed = false;
        let flat = flatten_mission_steps(mission);
        let normal_paths = flat
            .iter()
            .filter(|step| !step.spec.finally)
            .map(|step| step.spec.path.as_str())
            .collect::<BTreeSet<_>>();
        let views = run
            .steps
            .iter()
            .map(|step| (step.step.as_str(), step))
            .collect::<HashMap<_, _>>();
        if run.phase == "normal" {
            let admitted = flat.iter().filter(|step| !step.spec.finally).any(|step| {
                views.get(step.spec.path.as_str()).is_some_and(|view| {
                    view.attempt > 1
                        || matches!(
                            view.status.as_str(),
                            "ready"
                                | "claimed"
                                | "working"
                                | "verifying"
                                | "completed"
                                | "failed"
                                | "cancelled"
                        )
                })
            });
            if !admitted {
                let variables = crate::store::mission_run_variables(run, &run.revision);
                for baseline in &mission.baselines {
                    for gate in &baseline.gates {
                        if !matches!(
                            self.evaluate_context_gate(
                                run,
                                &run.subject,
                                &mission.id,
                                &mission.revision,
                                1,
                                gate,
                                &variables,
                            )?,
                            GateOutcome::Pass
                        ) {
                            changed |= self.store.set_mission_run_state(
                                &run.id,
                                "blocked",
                                "normal",
                                Some(&format!(
                                    "mission baseline `{}` does not hold",
                                    baseline.name
                                )),
                            )?;
                            return Ok(changed);
                        }
                    }
                }
            }
            let blocked_by_baseline = run.status == "blocked"
                && self
                    .store
                    .latest_claim(&run.subject, Some("mission-run.state"))?
                    .and_then(|claim| {
                        claim
                            .body
                            .pointer("/fields/reason")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .is_some_and(|reason| reason.starts_with("mission baseline `"));
            if blocked_by_baseline {
                changed |= self
                    .store
                    .set_mission_run_state(&run.id, "running", "normal", None)?;
            }
        }
        if mission.declarations_kdl.is_some() {
            let already_materialized = self
                .materialized_mission_generations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&run.generation);
            if !already_materialized {
                changed |= self.materialize_mission_declarations(run, mission)?;
                self.materialized_mission_generations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(run.generation.clone());
            }
        }
        let predecessor_retired = self
            .retired_predecessor_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&run.generation);
        if !predecessor_retired {
            let (retired, complete) = self.retire_predecessor_generation(run)?;
            changed |= retired;
            if complete {
                self.retired_predecessor_generations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(run.generation.clone());
            }
        }
        let mut normal_failed = flat.iter().any(|step| {
            !step.spec.finally
                && views.get(step.spec.path.as_str()).is_some_and(|view| {
                    (view.status == "failed" && view.attempt >= step.spec.retry.attempts)
                        || view.status == "cancelled"
                })
        });
        let mut normal_failure_reason = normal_failed.then(|| "a normal step failed".to_owned());
        let completion_selected = run.phase == "normal"
            && ((normal_failed && mission.completion.is_some())
                || self.mission_completion_selected(run, mission, &views)?);
        if completion_selected && !normal_failed {
            let variables = crate::store::mission_run_variables(run, &run.revision);
            if !self.products_hold_with_variables(&mission.products, &variables)? {
                return Ok(changed);
            }
            for gate in &mission.gates {
                match self.evaluate_context_gate(
                    run,
                    &run.subject,
                    &mission.id,
                    &mission.revision,
                    1,
                    gate,
                    &variables,
                )? {
                    GateOutcome::Pass => {}
                    GateOutcome::Pending => return Ok(changed),
                    GateOutcome::Fail(reason) => {
                        normal_failed = true;
                        normal_failure_reason = Some(reason);
                        break;
                    }
                }
            }
        }
        if completion_selected {
            for step in flat.iter().filter(|step| !step.spec.finally) {
                // A step whose run has not reached this host yet has nothing to cancel.
                let Some(view) = views.get(step.spec.path.as_str()) else {
                    continue;
                };
                if !matches!(view.status.as_str(), "completed" | "failed" | "cancelled") {
                    changed |= self.store.set_step_state(
                        &view.subject,
                        "cancelled",
                        Some(if normal_failed {
                            "another step failed"
                        } else {
                            "the normal phase completed"
                        }),
                    )?;
                }
            }
            if flat.iter().any(|step| step.spec.finally) {
                changed |= self.store.set_mission_run_state(
                    &run.id,
                    "running",
                    "final",
                    normal_failure_reason.as_deref(),
                )?;
            } else {
                changed |= self.store.set_mission_run_state(
                    &run.id,
                    "running",
                    if normal_failed {
                        "cleanup-failed"
                    } else {
                        "cleanup-completed"
                    },
                    normal_failure_reason.as_deref(),
                )?;
            }
            return Ok(changed);
        }

        if matches!(run.phase.as_str(), "final" | "final-cancelled") {
            let final_terminal = flat.iter().filter(|step| step.spec.finally).all(|step| {
                views.get(step.spec.path.as_str()).is_some_and(|view| {
                    matches!(view.status.as_str(), "completed" | "failed" | "cancelled")
                })
            });
            if final_terminal {
                let final_failed = flat.iter().filter(|step| step.spec.finally).any(|step| {
                    views
                        .get(step.spec.path.as_str())
                        .is_some_and(|view| view.status == "failed")
                });
                let failed = run.phase != "final-cancelled"
                    && run.steps.iter().any(|step| {
                        !step.step.is_empty()
                            && matches!(step.status.as_str(), "failed" | "cancelled")
                    });
                let terminal_status = if final_failed || failed {
                    "failed"
                } else if run.phase == "final-cancelled" {
                    "cancelled"
                } else {
                    "completed"
                };
                changed |= self.store.set_mission_run_state(
                    &run.id,
                    "running",
                    &format!("cleanup-{terminal_status}"),
                    (final_failed || failed).then_some("one or more mission steps failed"),
                )?;
                return Ok(changed);
            }
        }

        for step in flat {
            let Some(view) = views.get(step.spec.path.as_str()).copied() else {
                continue;
            };
            // One step that fails records a fault on that step. The run's other steps and its
            // status are still evaluated.
            changed |= self
                .isolate("step", &view.subject, || -> Result<bool> {
                    let mut changed = false;
                    let eligible_phase = ((run.phase == "normal"
                        || run.phase == "revision-draining")
                        && !step.spec.finally)
                        || (matches!(run.phase.as_str(), "final" | "final-cancelled")
                            && step.spec.finally);
                    if !eligible_phase {
                        return Ok(changed);
                    }
                    if run.phase == "revision-draining"
                        && matches!(
                            view.status.as_str(),
                            "pending" | "ready" | "blocked" | "failed" | "completed" | "cancelled"
                        )
                    {
                        return Ok(changed);
                    }
                    if view.status == "failed" {
                        if view.attempt < step.spec.retry.attempts {
                            changed |= self.store.retry_step(
                                &view.subject,
                                "the step repeat policy permits another attempt",
                                step.spec.retry.backoff_ms,
                            )?;
                        }
                        return Ok(changed);
                    }
                    if matches!(view.status.as_str(), "completed" | "cancelled") {
                        return Ok(changed);
                    }
                    if view.status == "orphaned" {
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "ready",
                            Some("the prior worker incarnation ended"),
                        )?;
                        return Ok(changed);
                    }
                    if matches!(
                        view.status.as_str(),
                        "claimed" | "working" | "verifying" | "blocked"
                    ) && self.store.work_claim_is_orphaned(view)?
                    {
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "orphaned",
                            Some("the exact worker incarnation ended"),
                        )?;
                        return Ok(changed);
                    }
                    if view.status == "ready"
                        && view.blocked_reason.as_deref() == Some("the worker lease expired")
                    {
                        if self.step_timed_out(view, &step)? {
                            changed |= self.store.set_step_state(
                                &view.subject,
                                "failed",
                                Some("the active execution timeout expired"),
                            )?;
                            return Ok(changed);
                        }
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "ready",
                            Some("the worker lease expired"),
                        )?;
                        return Ok(changed);
                    }
                    if let Some(expiry) = view.claim_expires_at_unix_ms
                        && expiry <= now_ms()
                        && matches!(
                            view.status.as_str(),
                            "claimed" | "working" | "verifying" | "blocked"
                        )
                    {
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "ready",
                            Some("the worker lease expired"),
                        )?;
                        return Ok(changed);
                    }
                    let assignment_blocked = view.status == "blocked"
                        && view.blocked_reason.as_deref().is_some_and(|reason| {
                            reason.starts_with("no eligible agent is present")
                        });
                    let baseline_blocked = view.status == "blocked"
                        && view
                            .blocked_reason
                            .as_deref()
                            .is_some_and(|reason| reason.starts_with("step baseline `"));
                    if view.status == "pending" || assignment_blocked || baseline_blocked {
                        if view
                            .not_before_unix_ms
                            .is_some_and(|not_before| not_before > now_ms())
                        {
                            return Ok(changed);
                        }
                        if !self.step_dependencies_hold(run, &step, &views)? {
                            return Ok(changed);
                        }
                        let mut baseline_holds = true;
                        for baseline in &step.spec.baselines {
                            for gate in &baseline.gates {
                                if !matches!(
                                    self.evaluate_mission_gate(run, &step, view, gate)?,
                                    GateOutcome::Pass
                                ) {
                                    changed |= self.store.set_step_state(
                                        &view.subject,
                                        "blocked",
                                        Some(&format!(
                                            "step baseline `{}` does not hold",
                                            baseline.name
                                        )),
                                    )?;
                                    baseline_holds = false;
                                    break;
                                }
                            }
                            if !baseline_holds {
                                break;
                            }
                        }
                        if !baseline_holds {
                            return Ok(changed);
                        }
                        if baseline_blocked {
                            changed |= self.store.set_step_state(&view.subject, "pending", None)?;
                        }
                        let eligible_agent = || -> Result<bool> {
                            Ok(view
                                .assigned_to
                                .iter()
                                .chain(view.available_to.iter())
                                .map(|agent| self.store.selected_desired_kind(agent))
                                .collect::<Result<Vec<_>>>()?
                                .iter()
                                .any(|kind| kind.as_deref() == Some("agent")))
                        };
                        let mut eligible = view.agentless || eligible_agent()?;
                        if !eligible {
                            // A step can declare its own assigned agent. Create only that agent before
                            // checking eligibility; other declarations still wait for active execution.
                            changed |=
                                self.materialize_step_declarations(run, &step, view, true)?;
                            eligible = eligible_agent()?;
                        }
                        if !eligible {
                            let eligible = view
                                .assigned_to
                                .iter()
                                .chain(view.available_to.iter())
                                .map(|agent| format!("`{agent}`"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            changed |= self.store.set_step_state(
                                &view.subject,
                                "blocked",
                                Some(&format!(
                                    "no eligible agent is present in the desired graph: {eligible}"
                                )),
                            )?;
                            return Ok(changed);
                        }
                        changed |= self.store.set_step_state(&view.subject, "ready", None)?;
                        return Ok(changed);
                    }
                    if !matches!(
                        view.status.as_str(),
                        "ready" | "claimed" | "working" | "verifying" | "blocked"
                    ) {
                        return Ok(changed);
                    }
                    // Agentless work has no worker claim to open its execution interval. Admit every
                    // eligible agentless step automatically before materializing declarations or waiting
                    // on gates, so its timeout and lifecycle are real instead of remaining `ready`
                    // forever. Nested missions need the persisted parent transition before their children
                    // can be evaluated; other agentless work can materialize in this same pass.
                    if view.status == "ready" && view.agentless {
                        let reason = awaited_run(run, &step, view)?
                            .map(|after| format!("waiting for `{after}` to complete"));
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "working",
                            reason.as_deref(),
                        )?;
                        if step.spec.nested_mission.is_some() {
                            return Ok(changed);
                        }
                    }
                    changed |= self.materialize_step_declarations(run, &step, view, false)?;
                    if let Some(reason) = self.step_declaration_failure(&view.subject)? {
                        changed |=
                            self.store
                                .set_step_state(&view.subject, "failed", Some(&reason))?;
                        return Ok(changed);
                    }
                    if self.step_timed_out(view, &step)? {
                        changed |= self.store.set_step_state(
                            &view.subject,
                            "failed",
                            Some("the active execution timeout expired"),
                        )?;
                        return Ok(changed);
                    }
                    if !self.step_declarations_hold(&view.subject)? {
                        return Ok(changed);
                    }
                    if !view.agentless && !view.worker_reported {
                        return Ok(changed);
                    }
                    if let Some(loop_spec) = &step.spec.loop_spec {
                        changed |= self.evaluate_loop_step(run, &step, view, loop_spec)?;
                        return Ok(changed);
                    }
                    if let Some(nested) = &step.spec.nested_mission {
                        let nested_prefix = format!("{}/{}/", step.spec.path, nested.id);
                        if !views
                            .iter()
                            .filter(|(path, _)| path.starts_with(&nested_prefix))
                            .all(|(_, child)| child.status == "completed")
                        {
                            return Ok(changed);
                        }
                    }
                    if step.spec.produces_mission.is_some()
                        && !self.produced_mission_holds(step.spec, view)?
                    {
                        return Ok(changed);
                    }
                    if step.spec.uses_mission.is_some() {
                        let (use_changed, outcome) =
                            self.evaluate_used_mission(run, &step, view, &views)?;
                        changed |= use_changed;
                        match outcome {
                            UsedMissionOutcome::Pending => return Ok(changed),
                            UsedMissionOutcome::Completed => {}
                            UsedMissionOutcome::Failed(reason) => {
                                changed |= self.store.set_step_state(
                                    &view.subject,
                                    "failed",
                                    Some(&reason),
                                )?;
                                return Ok(changed);
                            }
                        }
                    }
                    if let Some(after) = awaited_run(run, &step, view)? {
                        match self.store.mission_run_status(&after)?.as_deref() {
                            Some("completed") => {}
                            Some(status @ ("failed" | "cancelled")) => {
                                changed |= self.store.set_step_state(
                                    &view.subject,
                                    "failed",
                                    Some(&format!("the awaited mission run `{after}` is {status}")),
                                )?;
                                return Ok(changed);
                            }
                            _ => return Ok(changed),
                        }
                    }
                    if let Some(missing) = self.missing_product(run, &step, view)? {
                        changed |= self.notify_missing_product(view, &missing)?;
                        return Ok(changed);
                    }
                    let mut gates_pass = true;
                    for gate in &step.spec.gates {
                        match self.evaluate_mission_gate(run, &step, view, gate)? {
                            GateOutcome::Pass => {}
                            GateOutcome::Pending => {
                                gates_pass = false;
                                break;
                            }
                            GateOutcome::Fail(reason) => {
                                changed |= self.store.set_step_state(
                                    &view.subject,
                                    "failed",
                                    Some(&reason),
                                )?;
                                gates_pass = false;
                                break;
                            }
                        }
                    }
                    if gates_pass {
                        changed |= self
                            .store
                            .set_step_state(&view.subject, "completed", None)?;
                    }
                    Ok(changed)
                })
                .unwrap_or(false);
        }
        if run.phase == "normal" {
            let refreshed = self
                .store
                .mission_run(&run.id)?
                .context("the active mission run disappeared")?;
            let normal = refreshed
                .steps
                .iter()
                .filter(|view| normal_paths.contains(view.step.as_str()))
                .collect::<Vec<_>>();
            let advancing = normal.iter().any(|view| {
                matches!(
                    view.status.as_str(),
                    "ready" | "claimed" | "working" | "verifying"
                )
            });
            let failed = normal
                .iter()
                .any(|view| matches!(view.status.as_str(), "failed" | "cancelled"));
            let (status, reason) = if advancing {
                ("running", None)
            } else if failed {
                ("blocked", Some("the mission has no available step"))
            } else if mission.completion.is_none() && !changed {
                ("standing", Some("the open mission has no available step"))
            } else {
                ("running", None)
            };
            changed |= self
                .store
                .set_mission_run_state(&run.id, status, "normal", reason)?;
        }
        Ok(changed)
    }

    fn reconcile_mission_run_cleanup(&self, run: &MissionRunView) -> Result<bool> {
        let owner_runs = if run.mode == "eval" {
            self.store
                .mission_runs_for_root(&run.id)?
                .into_iter()
                .map(|owned_run| owned_run.subject)
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::from([run.subject.clone()])
        };
        let owned = owner_runs
            .iter()
            .map(|owner| self.store.desired_subjects_for_owner_run(owner))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let intake_stopped = self.stop_owned_intake(
            &owned.iter().collect::<Vec<_>>(),
            None,
            &format!("cleanup-mission-run-intake:{}", run.generation),
        )?;
        let owned = owned
            .into_iter()
            .filter(|subject| subject.member.is_some() || subject.kind == "stop")
            .collect::<Vec<_>>();
        let mut live = Vec::new();
        for subject in &owned {
            let actual = self.store.latest_actual_value(&subject.subject)?;
            // A stop declaration for a runtime that never existed has no process
            // to await. Otherwise cleanup can remain pending forever.
            if subject.kind == "stop" && actual.is_none() {
                continue;
            }
            let status = actual
                .as_ref()
                .and_then(|actual| actual_field(actual, "status"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if !matches!(status.as_deref(), Some("stopped" | "absent" | "exited")) {
                live.push(subject);
            }
        }
        let running = live
            .iter()
            .filter(|subject| subject.kind != "stop")
            .map(|subject| format!("  stop {:?}", subject.subject))
            .collect::<Vec<_>>();
        if !running.is_empty() {
            // Stop each owned subject exactly. Execution parsing would scope a
            // top-level seat that an eval owns under the run, and that stop
            // would never reach the seat.
            let source = format!("version 2\n\n{}\n", running.join("\n"));
            let mut intent = crate::graph::parse_internal_intent(&source, &self.host)?;
            for stop in intent.subjects.values_mut() {
                stop.owner_run = live
                    .iter()
                    .find(|subject| subject.subject == stop.subject)
                    .and_then(|subject| subject.owner_run.clone());
            }
            let response = self
                .store
                .apply_internal(&intent, &format!("cleanup-mission-run:{}", run.generation))?;
            return Ok(response.changed || intake_stopped);
        }
        // Cleanup waits for each owned runtime to stop, but not forever. A runtime on a host that
        // never answers, or one that cannot be killed, would otherwise hold the run and its slot.
        // Its stop declaration stays, so stopping continues after the run ends.
        let mut unstopped = None;
        if !live.is_empty() {
            let since = self
                .store
                .latest_claim(&run.subject, Some("mission-run.state"))?
                .filter(|claim| {
                    claim.body.pointer("/fields/phase").and_then(Value::as_str)
                        == Some(run.phase.as_str())
                })
                .map_or_else(now_ms, |claim| claim.accepted_at_unix_ms);
            let deadline = since.saturating_add(self.cleanup_deadline.as_millis());
            if now_ms() < deadline {
                self.arm_restart(&format!("cleanup:{}", run.subject), deadline);
                return Ok(intake_stopped);
            }
            unstopped = Some(format!(
                "cleanup ended at its deadline with runtimes still live: {}",
                live.iter()
                    .map(|subject| subject.subject.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut status = run.phase.strip_prefix("cleanup-").unwrap_or("failed");
        let mut changed = intake_stopped;
        if run.mode == "eval" {
            let run_failure_reason = self
                .store
                .latest_claim(&run.subject, Some("mission-run.state"))?
                .and_then(|claim| {
                    claim
                        .body
                        .pointer("/fields/reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            let mut runtime_cleanup_errors = unstopped.iter().cloned().collect::<Vec<_>>();
            match self.store.eval_runtime_records(&run.subject) {
                Ok(records) => {
                    for (runtime_id, terminal) in records {
                        if let Err(error) = self.runtime.remove(&runtime_id, terminal) {
                            runtime_cleanup_errors.push(format!("{runtime_id}: {error}"));
                        }
                    }
                }
                Err(error) => {
                    runtime_cleanup_errors.push(error.to_string());
                }
            }
            let (verdict, reason, residue) = match (
                runtime_cleanup_errors.is_empty(),
                self.store.retire_eval_owned_desired(&run.subject),
            ) {
                (false, retired) => {
                    status = "cancelled";
                    let mut reasons = runtime_cleanup_errors;
                    if let Err(error) = retired {
                        reasons.push(error.to_string());
                    }
                    (
                        "void",
                        Some(format!(
                            "eval cleanup infrastructure failed: {}",
                            reasons.join("; ")
                        )),
                        Vec::new(),
                    )
                }
                (true, Ok(residue)) if residue.is_empty() => (
                    match status {
                        "completed" => "pass",
                        "failed" => "fail",
                        _ => "void",
                    },
                    (status == "failed").then_some(run_failure_reason).flatten(),
                    residue,
                ),
                (true, Ok(residue)) => {
                    status = "failed";
                    (
                        "fail",
                        Some("the eval left subjects in its owned graph".to_owned()),
                        residue,
                    )
                }
                (true, Err(error)) => {
                    status = "cancelled";
                    (
                        "void",
                        Some(format!("eval cleanup infrastructure failed: {error}")),
                        Vec::new(),
                    )
                }
            };
            let mut fields = BTreeMap::from([("verdict".into(), Value::String(verdict.into()))]);
            if let Some(reason) = reason {
                fields.insert("reason".into(), Value::String(reason));
            }
            if !residue.is_empty() {
                fields.insert(
                    "residue".into(),
                    Value::Array(residue.into_iter().map(Value::String).collect()),
                );
            }
            self.record_once(&run.subject, "eval.verdict", fields)?;
            changed = true;
        }
        changed |=
            self.store
                .set_mission_run_state(&run.id, status, "terminal", unstopped.as_deref())?;
        Ok(changed)
    }

    fn mission_completion_selected(
        &self,
        run: &MissionRunView,
        mission: &MissionSpec,
        views: &HashMap<&str, &crate::model::StepRunView>,
    ) -> Result<bool> {
        let Some(completion) = &mission.completion else {
            return Ok(false);
        };
        match completion {
            crate::model::CompletionSpec::AllStepsExhausted => Ok(flatten_mission_steps(mission)
                .into_iter()
                .filter(|step| !step.spec.finally)
                .all(|step| {
                    views.get(step.spec.path.as_str()).is_some_and(|view| {
                        view.status == "completed"
                            || view.status == "cancelled"
                            || (view.status == "failed" && view.attempt >= step.spec.retry.attempts)
                    })
                })),
            crate::model::CompletionSpec::Dependencies { dependencies } => {
                let variables = crate::store::mission_run_variables(run, &run.revision);
                for dependency in dependencies {
                    let holds = match dependency {
                        DependencySpec::Step { step, state } => views
                            .get(step.as_str())
                            .is_some_and(|view| match state.as_str() {
                                "completed" => view.status == "completed",
                                "failed" => view.status == "failed",
                                "terminal" => matches!(
                                    view.status.as_str(),
                                    "completed" | "failed" | "cancelled"
                                ),
                                _ => false,
                            }),
                        DependencySpec::Predicate { gate } => matches!(
                            self.evaluate_context_gate(
                                run,
                                &run.subject,
                                &mission.id,
                                &mission.revision,
                                1,
                                gate,
                                &variables,
                            )?,
                            GateOutcome::Pass
                        ),
                    };
                    if !holds {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    fn produced_mission_holds(
        &self,
        step: &StepSpec,
        view: &crate::model::StepRunView,
    ) -> Result<bool> {
        let Some(expected) = &step.produces_mission else {
            return Ok(true);
        };
        let Some(output) =
            self.store
                .mission_output(&view.subject, view.attempt, &view.definition_hash)?
        else {
            return Ok(false);
        };
        if output
            .mission
            .strip_prefix("mission/")
            .unwrap_or(&output.mission)
            != expected
        {
            return Ok(false);
        }
        Ok(self
            .store
            .mission_spec(expected, Some(&output.revision))?
            .is_some_and(|mission| mission.state == MissionState::Ready))
    }

    fn evaluate_used_mission(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        views: &HashMap<&str, &crate::model::StepRunView>,
    ) -> Result<(bool, UsedMissionOutcome)> {
        let (mission, revision) = match step
            .spec
            .uses_mission
            .as_ref()
            .expect("a used mission was checked")
        {
            UsedMissionSpec::Revision { mission, revision } => (mission.clone(), revision.clone()),
            UsedMissionSpec::StepOutput { step: producer } => {
                let path = if step.dependency_prefix.is_empty() {
                    producer.clone()
                } else {
                    format!("{}/{}", step.dependency_prefix, producer)
                };
                let Some(producer) = views.get(path.as_str()).copied() else {
                    return Ok((false, UsedMissionOutcome::Pending));
                };
                let Some(output) = self.store.mission_output(
                    &producer.subject,
                    producer.attempt,
                    &producer.definition_hash,
                )?
                else {
                    return Ok((false, UsedMissionOutcome::Pending));
                };
                (
                    output
                        .mission
                        .strip_prefix("mission/")
                        .unwrap_or(&output.mission)
                        .to_owned(),
                    output.revision,
                )
            }
        };
        let Some(selected) = self.store.mission_spec(&mission, Some(&revision))? else {
            return Ok((
                false,
                UsedMissionOutcome::Failed(format!(
                    "the exact used mission `mission/{mission}@{revision}` is unavailable"
                )),
            ));
        };
        if selected.state != MissionState::Ready {
            return Ok((
                false,
                UsedMissionOutcome::Failed(format!(
                    "the exact used mission `mission/{mission}@{revision}` is not ready"
                )),
            ));
        }
        let (child, changed) =
            if let Some(child) = self.store.mission_run_for_parent_step(&view.subject)? {
                (child, false)
            } else {
                let selector = step_run_selector(view);
                let child = match self.store.create_child_mission_run(
                    &MissionRunRequest {
                        mission: mission.clone(),
                        revision: Some(revision.clone()),
                        workspace: run.workspace.clone(),
                        requester: Some(run.requester.clone()),
                        mode: Some(run.mode.clone()),
                        inputs: BTreeMap::new(),
                        idempotency_key: format!(
                            "uses-mission:{}:{}:mission/{mission}@{revision}",
                            view.subject, view.attempt
                        ),
                    },
                    run,
                    &view.subject,
                    Some(&selector),
                ) {
                    Ok(child) => child,
                    Err(error) if error.code == "mission-run-capacity" => {
                        return Ok((false, UsedMissionOutcome::Pending));
                    }
                    Err(error) => return Err(error.into()),
                };
                (child, true)
            };
        if child.mission != format!("mission/{mission}") || child.revision != revision {
            return Ok((
                changed,
                UsedMissionOutcome::Failed(
                    "the step already started a different exact mission revision".into(),
                ),
            ));
        }
        let outcome = match child.status.as_str() {
            "completed" => UsedMissionOutcome::Completed,
            "failed" | "cancelled" => UsedMissionOutcome::Failed(format!(
                "the used mission run `{}` is {}",
                child.subject, child.status
            )),
            _ => UsedMissionOutcome::Pending,
        };
        Ok((changed, outcome))
    }

    fn evaluate_loop_step(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
    ) -> Result<bool> {
        let loop_subject = format!(
            "loop-run/{}/{}",
            run.generation
                .strip_prefix("run-generation/")
                .unwrap_or(&run.generation),
            loop_spec.path
        );
        let mut variables = run_variables(run, step, view);
        variables.insert("ST_LOOP_ROUND".into(), view.attempt.to_string());
        variables.insert("loop.round".into(), view.attempt.to_string());
        let prior_feedback = self
            .store
            .claims_for(&loop_subject, Some("loop.round-result"))?
            .last()
            .and_then(|claim| claim.body.pointer("/fields/feedback"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        variables.insert("ST_LOOP_FEEDBACK".into(), prior_feedback.clone());
        variables.insert("loop.feedback".into(), prior_feedback.clone());
        variables.insert("ST_LOOP_ITEM_ID".into(), String::new());
        // Every pass enters here. A later write in the same round (a gate wait, a winner, a
        // reschedule) already says the round is running, so a bare entry claim would only
        // replace it, and the next pass would write that later claim again.
        let round_running = self
            .store
            .latest_claim(&loop_subject, Some("loop.state"))?
            .is_some_and(|claim| {
                claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("running")
                    && claim.body.pointer("/fields/round").and_then(Value::as_u64)
                        == Some(u64::from(view.attempt))
            });
        if !round_running {
            self.record_once(
                &loop_subject,
                "loop.state",
                BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("round".into(), Value::from(view.attempt)),
                ]),
            )?;
        }
        let first_execution = self.loop_first_execution_at(run, view)?;
        let timed_out = loop_spec.timeout_ms.is_some_and(|timeout| {
            first_execution
                .is_some_and(|started| now_ms().saturating_sub(started) >= timeout as u128)
        });
        if timed_out {
            return self.finish_exhausted_loop(
                run,
                view,
                loop_spec,
                &loop_subject,
                &variables,
                "the loop timeout expired",
            );
        }
        if loop_spec.for_each.is_some() {
            return self.evaluate_for_each_loop(
                run,
                step,
                view,
                loop_spec,
                &loop_subject,
                &variables,
            );
        }
        if loop_spec.candidates.is_some() {
            return self.evaluate_candidate_loop(
                run,
                step,
                view,
                loop_spec,
                &loop_subject,
                &variables,
            );
        }
        let dispatch = self.loop_dispatch_count(&loop_subject, view.attempt, None, None)?;
        let base_key = format!("loop-round:{}:{}", view.subject, view.attempt);
        let key = if dispatch == 0 {
            base_key
        } else {
            format!("{base_key}:dispatch-{dispatch}")
        };
        let expected = self.store.mission_run_subject_for_idempotency_key(&key);
        let existed = self.store.mission_run(&expected)?.is_some();
        let mut inputs: BTreeMap<String, String> = run
            .inputs
            .iter()
            .map(|(name, input)| {
                let value = match input.kind {
                    MissionInputKind::Text => input.value.clone(),
                    MissionInputKind::Resource => match (&input.subject, &input.claim_id) {
                        (Some(subject), Some(claim)) => format!("{subject}@{claim}"),
                        _ => input.value.clone(),
                    },
                };
                (name.clone(), value)
            })
            .collect();
        inputs.insert(LOOP_ROUND_INPUT.into(), view.attempt.to_string());
        inputs.insert(LOOP_FEEDBACK_INPUT.into(), prior_feedback);
        inputs.insert(LOOP_ITEM_INPUT.into(), "null".into());
        inputs.insert(CANDIDATE_INDEX_INPUT.into(), String::new());
        let child = if existed {
            self.store
                .mission_run(&expected)?
                .context("the existing loop round disappeared")?
        } else {
            match self.store.create_child_mission_run(
                &MissionRunRequest {
                    mission: loop_spec.round.id.clone(),
                    revision: Some(loop_spec.round.revision.clone()),
                    workspace: run.workspace.clone(),
                    requester: Some(run.requester.clone()),
                    mode: Some(run.mode.clone()),
                    inputs,
                    idempotency_key: key,
                },
                run,
                &view.subject,
                None,
            ) {
                Ok(_) => return Ok(true),
                Err(error) if error.code == "mission-run-capacity" => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        };
        match child.status.as_str() {
            "running" | "standing" | "blocked" => Ok(false),
            "failed" | "cancelled" => {
                if self.loop_child_timed_out_without_claim(&child)? {
                    return self.reschedule_unclaimed_loop_child(
                        &loop_subject,
                        view.attempt,
                        dispatch,
                        &child,
                        None,
                        None,
                    );
                }
                let token_usage = self.mission_run_token_usage(&child)?;
                let structural = self.loop_child_failure_is_structural(&child)?;
                let failure_reason = if structural {
                    "the loop round mission had a structural failure"
                } else {
                    "the loop round mission failed"
                };
                self.record_loop_round(
                    &loop_subject,
                    view.attempt,
                    &child,
                    "failed",
                    &BTreeMap::new(),
                    None,
                    None,
                    None,
                    token_usage,
                    Some(failure_reason),
                )?;
                if structural {
                    self.record_once(
                        &loop_subject,
                        "loop.state",
                        BTreeMap::from([
                            ("status".into(), Value::String("failed".into())),
                            ("round".into(), Value::from(view.attempt)),
                            ("reason".into(), Value::String(failure_reason.into())),
                        ]),
                    )?;
                    return self.store.set_step_state(
                        &view.subject,
                        "cancelled",
                        Some(failure_reason),
                    );
                }
                if self.repeated_loop_failures(&loop_subject)?
                    >= loop_spec.stop.repeated_failure.unwrap_or(u32::MAX)
                {
                    return self.finish_exhausted_loop(
                        run,
                        view,
                        loop_spec,
                        &loop_subject,
                        &variables,
                        "the repeated-failure limit was reached",
                    );
                }
                if view.attempt >= loop_spec.max_rounds {
                    self.finish_exhausted_loop(
                        run,
                        view,
                        loop_spec,
                        &loop_subject,
                        &variables,
                        "the last loop round failed",
                    )
                } else {
                    self.store.set_step_state(
                        &view.subject,
                        "failed",
                        Some("the loop round failed and another round is available"),
                    )
                }
            }
            "completed" => {
                let metrics = match self.evaluate_loop_metrics(
                    run,
                    step,
                    view,
                    loop_spec,
                    &loop_subject,
                    &variables,
                ) {
                    Ok(Some(metrics)) => metrics,
                    Ok(None) => return Ok(false),
                    Err(error) => {
                        return self.store.set_step_state(
                            &view.subject,
                            "cancelled",
                            Some(&format!("the loop metric failed: {error:#}")),
                        );
                    }
                };
                let keep = self.loop_round_improves(&loop_subject, loop_spec, &metrics)?;
                let feedback = self.write_loop_feedback(
                    &loop_subject,
                    view.attempt,
                    &child,
                    &metrics,
                    keep,
                    None,
                )?;
                match self
                    .evaluate_loop_branch(run, view, loop_spec, keep, &feedback, None, None)?
                {
                    LoopBranchOutcome::Pending => return Ok(false),
                    LoopBranchOutcome::Failed(reason) => {
                        self.record_loop_round(
                            &loop_subject,
                            view.attempt,
                            &child,
                            "failed",
                            &metrics,
                            Some(&feedback),
                            None,
                            None,
                            self.mission_run_token_usage(&child)?,
                            Some(&reason),
                        )?;
                        return self.store.set_step_state(
                            &view.subject,
                            "cancelled",
                            Some(&reason),
                        );
                    }
                    LoopBranchOutcome::Completed => {}
                }
                let round_status = if keep { "completed" } else { "discarded" };
                let token_usage = self.mission_run_token_usage(&child)?;
                self.record_loop_round(
                    &loop_subject,
                    view.attempt,
                    &child,
                    round_status,
                    &metrics,
                    Some(&feedback),
                    None,
                    None,
                    token_usage,
                    None,
                )?;
                let (best_round, best_metrics) = self.loop_best(&loop_subject, loop_spec)?;
                self.record_once(
                    &loop_subject,
                    "loop.state",
                    BTreeMap::from([
                        ("status".into(), Value::String("running".into())),
                        ("round".into(), Value::from(view.attempt)),
                        ("best_round".into(), Value::from(best_round)),
                        ("best_metrics".into(), serde_json::to_value(&best_metrics)?),
                        ("feedback".into(), Value::String(feedback.clone())),
                    ]),
                )?;
                let mut passed = !loop_spec.until.is_empty();
                let mut waiting = false;
                for gate in &loop_spec.until {
                    match self.evaluate_context_gate(
                        run,
                        &loop_subject,
                        &loop_spec.id,
                        &step.spec.definition_hash,
                        view.attempt,
                        gate,
                        &variables,
                    )? {
                        GateOutcome::Pass => {}
                        GateOutcome::Pending
                            if matches!(
                                gate,
                                GateSpec::Mechanical { .. }
                                    | GateSpec::Llm { .. }
                                    | GateSpec::Human { .. }
                            ) =>
                        {
                            waiting = true;
                            passed = false;
                            break;
                        }
                        GateOutcome::Pending | GateOutcome::Fail(_) => passed = false,
                    }
                }
                if waiting {
                    return Ok(false);
                }
                if passed {
                    self.record_once(
                        &loop_subject,
                        "loop.state",
                        BTreeMap::from([
                            ("status".into(), Value::String("completed".into())),
                            ("round".into(), Value::from(view.attempt)),
                            ("best_round".into(), Value::from(best_round)),
                            ("best_metrics".into(), serde_json::to_value(&best_metrics)?),
                            ("feedback".into(), Value::String(feedback)),
                        ]),
                    )?;
                    return self.store.set_step_state(&view.subject, "completed", None);
                }
                if self.loop_stop_reason(&loop_subject, loop_spec)?.is_some() {
                    let reason = self
                        .loop_stop_reason(&loop_subject, loop_spec)?
                        .expect("the stop reason was present");
                    return self.finish_exhausted_loop(
                        run,
                        view,
                        loop_spec,
                        &loop_subject,
                        &variables,
                        &reason,
                    );
                }
                if view.attempt >= loop_spec.max_rounds {
                    self.finish_exhausted_loop(
                        run,
                        view,
                        loop_spec,
                        &loop_subject,
                        &variables,
                        "the loop reached max-rounds without satisfying until",
                    )
                } else {
                    self.store.set_step_state(
                        &view.subject,
                        "failed",
                        Some("the loop exit gates did not pass"),
                    )
                }
            }
            _ => Ok(false),
        }
    }

    fn evaluate_loop_metrics(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        variables: &BTreeMap<String, String>,
    ) -> Result<Option<BTreeMap<String, f64>>> {
        let mut values = BTreeMap::new();
        for metric in &loop_spec.metrics {
            let value = match &metric.source {
                MetricSource::Gate { gate } => {
                    let gate = loop_spec
                        .until
                        .iter()
                        .find(|candidate| crate::graph::gate_name(candidate) == gate)
                        .with_context(|| {
                            format!("metric `{}` names an unavailable gate", metric.name)
                        })?;
                    match self.evaluate_context_gate(
                        run,
                        loop_subject,
                        &loop_spec.id,
                        &step.spec.definition_hash,
                        view.attempt,
                        gate,
                        variables,
                    )? {
                        GateOutcome::Pass => 1.0,
                        GateOutcome::Pending
                            if matches!(
                                gate,
                                GateSpec::Mechanical { .. }
                                    | GateSpec::Llm { .. }
                                    | GateSpec::Human { .. }
                            ) =>
                        {
                            return Ok(None);
                        }
                        GateOutcome::Pending | GateOutcome::Fail(_) => 0.0,
                    }
                }
                MetricSource::Field { subject, path } => {
                    let subject = crate::mission::interpolate(subject, variables)?;
                    let path = crate::mission::interpolate(path, variables)?;
                    let Some(actual) = self.subject_value(&subject)? else {
                        return Ok(None);
                    };
                    actual_field(&actual, &path)
                        .and_then(Value::as_f64)
                        .filter(|value| value.is_finite())
                        .with_context(|| {
                            format!(
                                "metric `{}` did not find a finite number at `{path}` on `{subject}`",
                                metric.name
                            )
                        })?
                }
                MetricSource::Exec { .. } => {
                    let Some(value) =
                        self.evaluate_exec_metric(run, view, loop_subject, metric, variables)?
                    else {
                        return Ok(None);
                    };
                    value
                }
            };
            values.insert(metric.name.clone(), value);
        }
        Ok(Some(values))
    }

    fn evaluate_exec_metric(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        loop_subject: &str,
        metric: &crate::model::MetricSpec,
        variables: &BTreeMap<String, String>,
    ) -> Result<Option<f64>> {
        let MetricSource::Exec {
            command,
            host,
            workspace,
            environment,
            time_limit_ms,
        } = &metric.source
        else {
            unreachable!("the caller selected an exec metric")
        };
        let digest = hex::encode(sha2::Sha256::digest(
            format!("{loop_subject}:{}:{}", view.attempt, metric.name).as_bytes(),
        ));
        let subject = format!("gate-operation/loop-metric/{}", &digest[..32]);
        if let Some(result) = self.store.latest_claim(&subject, Some("gate.result"))? {
            return result
                .body
                .pointer("/fields/value")
                .and_then(Value::as_f64)
                .map(Some)
                .context("a completed loop metric has no numeric value");
        }
        let command = crate::mission::interpolate(command, variables)?;
        let host = crate::mission::interpolate(host, variables)?;
        let mut workspace = crate::mission::interpolate(workspace, variables)?;
        if Path::new(&workspace).is_relative() {
            workspace = Path::new(&run.workspace)
                .join(&workspace)
                .to_string_lossy()
                .into_owned();
        }
        let mut environment = environment
            .iter()
            .map(|(name, value)| Ok((name.clone(), crate::mission::interpolate(value, variables)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        environment.extend(variables.clone());
        if host != self.host {
            return Ok(None);
        }
        let runtime_id = subject.replace('/', ".");
        if let Some(request) = self.store.latest_claim(&subject, Some("gate.requested"))? {
            if now_ms().saturating_sub(request.accepted_at_unix_ms) >= *time_limit_ms as u128 {
                self.stop_gate_runner(&subject, true)?;
                anyhow::bail!("metric `{}` exceeded {}ms", metric.name, time_limit_ms);
            }
            match self.runtime.observe_exec(&runtime_id)? {
                Some(observation) if observation.status == "running" => {
                    self.arm_gate_poll();
                    return Ok(None);
                }
                Some(observation) if observation.status == "exited" => {
                    if self.gate_exit_code(&subject, &observation)? != Some(0) {
                        anyhow::bail!("metric `{}` exited unsuccessfully", metric.name);
                    }
                    let log = self
                        .runtime
                        .read_exec_log(&runtime_id)?
                        .context("the metric command has no output")?;
                    let value = log
                        .trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|value| value.is_finite())
                        .context("the metric command did not print one finite number")?;
                    self.record_once(
                        &subject,
                        "gate.result",
                        BTreeMap::from([
                            ("verdict".into(), Value::String("pass".into())),
                            ("value".into(), Value::from(value)),
                        ]),
                    )?;
                    return Ok(Some(value));
                }
                _ => {
                    self.arm_gate_poll();
                    return Ok(None);
                }
            }
        }
        self.record_once(
            &subject,
            "gate.requested",
            BTreeMap::from([
                ("status".into(), Value::String("requested".into())),
                ("runner".into(), Value::String("loop-metric".into())),
            ]),
        )?;
        let member = MemberSpec {
            kind: MemberKind::Exec,
            host,
            runtime_id,
            workspace: workspace.clone(),
            workspace_create: false,
            cwd: workspace,
            terminal: false,
            launch: LaunchSpec::Shell(command),
            environment,
            tags: BTreeMap::new(),
            display_name: Some(format!("loop metric {}", metric.name)),
            lifecycle: MemberLifecycle::Service,
            restart: RestartType::Never,
            restart_intensity: RestartIntensity::default(),
            shutdown_timeout_ms: 5_000,
            driver: Some("loop-metric".into()),
        };
        self.perform_start(
            &DesiredSubject {
                subject: subject.clone(),
                kind: "gate".into(),
                desired: Value::Null,
                member: Some(member.clone()),
                owner_run: Some(run.subject.clone()),
                owner_generation: Some(run.generation.clone()),
                owner_step: Some(view.subject.clone()),
            },
            &member,
            "the loop metric was requested",
        )?;
        self.arm_gate_poll();
        Ok(None)
    }

    fn loop_round_improves(
        &self,
        loop_subject: &str,
        loop_spec: &LoopSpec,
        metrics: &BTreeMap<String, f64>,
    ) -> Result<bool> {
        let Some(metric_name) = &loop_spec.keep_metric else {
            return Ok(true);
        };
        let metric = loop_spec
            .metrics
            .iter()
            .find(|metric| &metric.name == metric_name)
            .context("the keep metric disappeared")?;
        let Some(current) = metrics.get(metric_name) else {
            anyhow::bail!("the keep metric has no current value");
        };
        let prior = self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?
            .into_iter()
            .filter(|claim| {
                claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("completed")
            })
            .filter_map(|claim| {
                claim
                    .body
                    .pointer(&format!("/fields/metrics/{metric_name}"))
                    .and_then(Value::as_f64)
            })
            .next_back();
        Ok(prior.is_none_or(|prior| {
            if metric.direction == "higher" {
                current - prior >= metric.min_improvement
            } else {
                prior - current >= metric.min_improvement
            }
        }))
    }

    fn write_loop_feedback(
        &self,
        loop_subject: &str,
        round: u32,
        child: &MissionRunView,
        metrics: &BTreeMap<String, f64>,
        keep: bool,
        candidate: Option<u32>,
    ) -> Result<String> {
        let suffix = candidate.map_or_else(String::new, |value| format!("/candidate-{value}"));
        let name = format!(
            "doc/loop-feedback/{}/{round}{suffix}",
            loop_subject
                .strip_prefix("loop-run/")
                .unwrap_or(loop_subject),
        );
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "loop": loop_subject,
            "round": round,
            "mission_run": child.subject,
            "metrics": metrics,
            "decision": if keep { "keep" } else { "discard" },
            "candidate": candidate,
        }))?;
        let document = self.store.put_document(
            &name,
            &body,
            &None,
            &format!("loop-feedback:{loop_subject}:{round}:{candidate:?}"),
        )?;
        Ok(format!("{}@{}", document.name, document.hash))
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_loop_branch(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        keep: bool,
        feedback: &str,
        candidate: Option<u32>,
        item: Option<&Value>,
    ) -> Result<LoopBranchOutcome> {
        let branch = if keep {
            loop_spec.on_keep.as_deref()
        } else {
            loop_spec.on_discard.as_deref()
        };
        let Some(branch) = branch else {
            return Ok(LoopBranchOutcome::Completed);
        };
        let label = if keep { "keep" } else { "discard" };
        let key = format!(
            "loop-{label}:{}:{}:{}",
            view.subject,
            view.attempt,
            candidate.map_or_else(|| "round".into(), |value| format!("candidate-{value}"))
        );
        let subject = self.store.mission_run_subject_for_idempotency_key(&key);
        if let Some(child) = self.store.mission_run(&subject)? {
            return Ok(match child.status.as_str() {
                "completed" => LoopBranchOutcome::Completed,
                "failed" | "cancelled" => LoopBranchOutcome::Failed(format!(
                    "the loop {label} branch failed in `{}`",
                    child.subject
                )),
                _ => LoopBranchOutcome::Pending,
            });
        }
        let mut inputs = self.child_loop_inputs(run);
        inputs.insert(LOOP_ROUND_INPUT.into(), view.attempt.to_string());
        inputs.insert(LOOP_FEEDBACK_INPUT.into(), feedback.into());
        inputs.insert(
            CANDIDATE_INDEX_INPUT.into(),
            candidate.map_or_else(String::new, |value| value.to_string()),
        );
        inputs.insert(
            LOOP_ITEM_INPUT.into(),
            item.map_or_else(|| "null".into(), Value::to_string),
        );
        self.store.create_child_mission_run(
            &MissionRunRequest {
                mission: branch.id.clone(),
                revision: Some(branch.revision.clone()),
                workspace: run.workspace.clone(),
                requester: Some(run.requester.clone()),
                mode: Some(run.mode.clone()),
                inputs,
                idempotency_key: key,
            },
            run,
            &view.subject,
            None,
        )?;
        Ok(LoopBranchOutcome::Pending)
    }

    fn child_loop_inputs(&self, run: &MissionRunView) -> BTreeMap<String, String> {
        let mut inputs = run
            .inputs
            .iter()
            .filter(|(name, _)| !name.starts_with("__st3_"))
            .map(|(name, input)| {
                let value = match input.kind {
                    MissionInputKind::Text => input.value.clone(),
                    MissionInputKind::Resource => match (&input.subject, &input.claim_id) {
                        (Some(subject), Some(claim)) => format!("{subject}@{claim}"),
                        _ => input.value.clone(),
                    },
                };
                (name.clone(), value)
            })
            .collect::<BTreeMap<_, _>>();
        inputs.insert(LOOP_ROUND_INPUT.into(), String::new());
        inputs.insert(LOOP_FEEDBACK_INPUT.into(), String::new());
        inputs.insert(LOOP_ITEM_INPUT.into(), "null".into());
        inputs.insert(CANDIDATE_INDEX_INPUT.into(), String::new());
        inputs
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_loop_child(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        mission: &MissionSpec,
        key: String,
        round: u32,
        feedback: &str,
        item: Option<&Value>,
        candidate: Option<u32>,
    ) -> Result<(MissionRunView, bool)> {
        let subject = self.store.mission_run_subject_for_idempotency_key(&key);
        if let Some(child) = self.store.mission_run(&subject)? {
            return Ok((child, false));
        }
        let mut inputs = self.child_loop_inputs(run);
        inputs.insert(LOOP_ROUND_INPUT.into(), round.to_string());
        inputs.insert(LOOP_FEEDBACK_INPUT.into(), feedback.into());
        inputs.insert(
            LOOP_ITEM_INPUT.into(),
            item.map_or_else(|| "null".into(), Value::to_string),
        );
        inputs.insert(
            CANDIDATE_INDEX_INPUT.into(),
            candidate.map_or_else(String::new, |value| value.to_string()),
        );
        let child = self.store.create_child_mission_run(
            &MissionRunRequest {
                mission: mission.id.clone(),
                revision: Some(mission.revision.clone()),
                workspace: run.workspace.clone(),
                requester: Some(run.requester.clone()),
                mode: Some(run.mode.clone()),
                inputs,
                idempotency_key: key,
            },
            run,
            &view.subject,
            None,
        )?;
        Ok((child, true))
    }

    fn loop_result_claim(
        &self,
        loop_subject: &str,
        round: u32,
        candidate: Option<u32>,
    ) -> Result<Option<crate::model::ClaimRecord>> {
        Ok(self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?
            .into_iter()
            .find(|claim| {
                claim.body.pointer("/fields/round").and_then(Value::as_u64)
                    == Some(u64::from(round))
                    && claim
                        .body
                        .pointer("/fields/candidate")
                        .and_then(Value::as_u64)
                        == candidate.map(u64::from)
            }))
    }

    fn evaluate_for_each_loop(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        base_variables: &BTreeMap<String, String>,
    ) -> Result<bool> {
        let for_each = loop_spec
            .for_each
            .as_ref()
            .expect("the caller selected a for-each loop");
        let snapshot = self
            .store
            .claims_for(loop_subject, Some("loop.state"))?
            .into_iter()
            .find_map(|claim| claim.body.pointer("/fields/items").cloned());
        let items = if let Some(Value::Array(items)) = snapshot {
            items
        } else {
            let resource = crate::mission::interpolate(&for_each.resource, base_variables)?;
            let field = crate::mission::interpolate(&for_each.field, base_variables)?;
            let Some(actual) = self.subject_value(&resource)? else {
                return Ok(false);
            };
            let Some(items) = actual_field(&actual, &field)
                .and_then(Value::as_array)
                .cloned()
            else {
                return self.store.set_step_state(
                    &view.subject,
                    "cancelled",
                    Some("a for-each field must contain an array"),
                );
            };
            if items.len() > 100 || items.len() > loop_spec.max_rounds as usize {
                return self.store.set_step_state(
                    &view.subject,
                    "cancelled",
                    Some("the for-each snapshot exceeds max-rounds or 100 items"),
                );
            }
            let mut ids = BTreeSet::new();
            for item in &items {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    return self.store.set_step_state(
                        &view.subject,
                        "cancelled",
                        Some("each for-each item needs a string id"),
                    );
                };
                if !ids.insert(id.to_owned()) {
                    return self.store.set_step_state(
                        &view.subject,
                        "cancelled",
                        Some("the for-each snapshot contains duplicate item IDs"),
                    );
                }
            }
            self.record_once(
                loop_subject,
                "loop.state",
                BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("round".into(), Value::from(0)),
                    ("items".into(), Value::Array(items.clone())),
                ]),
            )?;
            return Ok(true);
        };
        let mut active = 0_u32;
        let mut completed = 0_usize;
        for (index, item) in items.iter().enumerate() {
            let round = index as u32 + 1;
            if self.loop_result_claim(loop_subject, round, None)?.is_some() {
                completed += 1;
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                return self.store.set_step_state(
                    &view.subject,
                    "cancelled",
                    Some("the stored for-each item has no ID"),
                );
            };
            let dispatch = self.loop_dispatch_count(loop_subject, round, None, Some(id))?;
            let base_key = format!("loop-item:{}:{id}", view.subject);
            let key = if dispatch == 0 {
                base_key
            } else {
                format!("{base_key}:dispatch-{dispatch}")
            };
            let subject = self.store.mission_run_subject_for_idempotency_key(&key);
            if let Some(child) = self.store.mission_run(&subject)? {
                match child.status.as_str() {
                    "running" | "standing" | "blocked" => active += 1,
                    "failed" | "cancelled" => {
                        if self.loop_child_timed_out_without_claim(&child)? {
                            return self.reschedule_unclaimed_loop_child(
                                loop_subject,
                                round,
                                dispatch,
                                &child,
                                None,
                                Some(id),
                            );
                        }
                        return self.store.set_step_state(
                            &view.subject,
                            "cancelled",
                            Some(&format!("the for-each item `{id}` failed")),
                        );
                    }
                    "completed" => {
                        let mut item_view = view.clone();
                        item_view.attempt = round;
                        let mut variables = base_variables.clone();
                        variables.insert("ST_LOOP_ROUND".into(), round.to_string());
                        variables.insert("loop.round".into(), round.to_string());
                        if let Some(fields) = item.as_object() {
                            for (name, value) in fields {
                                variables.insert(
                                    format!("loop.item.{name}"),
                                    value
                                        .as_str()
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| value.to_string()),
                                );
                            }
                        }
                        variables.insert("ST_LOOP_ITEM_ID".into(), id.into());
                        let metrics = match self.evaluate_loop_metrics(
                            run,
                            step,
                            &item_view,
                            loop_spec,
                            &format!("{loop_subject}/item/{id}"),
                            &variables,
                        ) {
                            Ok(Some(metrics)) => metrics,
                            Ok(None) => return Ok(false),
                            Err(error) => {
                                return self.store.set_step_state(
                                    &view.subject,
                                    "cancelled",
                                    Some(&format!("the loop metric failed: {error:#}")),
                                );
                            }
                        };
                        let feedback = self.write_loop_feedback(
                            loop_subject,
                            round,
                            &child,
                            &metrics,
                            true,
                            None,
                        )?;
                        match self.evaluate_loop_branch(
                            run,
                            &item_view,
                            loop_spec,
                            true,
                            &feedback,
                            None,
                            Some(item),
                        )? {
                            LoopBranchOutcome::Pending => return Ok(false),
                            LoopBranchOutcome::Failed(reason) => {
                                return self.store.set_step_state(
                                    &view.subject,
                                    "cancelled",
                                    Some(&reason),
                                );
                            }
                            LoopBranchOutcome::Completed => {}
                        }
                        self.record_loop_round(
                            loop_subject,
                            round,
                            &child,
                            "completed",
                            &metrics,
                            Some(&feedback),
                            None,
                            Some(item),
                            self.mission_run_token_usage(&child)?,
                            None,
                        )?;
                        if let Some(reason) = self.loop_stop_reason(loop_subject, loop_spec)? {
                            return self.finish_exhausted_loop(
                                run,
                                view,
                                loop_spec,
                                loop_subject,
                                &variables,
                                &reason,
                            );
                        }
                        completed += 1;
                    }
                    _ => {}
                }
                continue;
            }
            if active < for_each.max_parallel {
                self.ensure_loop_child(
                    run,
                    view,
                    &loop_spec.round,
                    key,
                    round,
                    "",
                    Some(item),
                    None,
                )?;
                active += 1;
            }
        }
        if completed != items.len() {
            return Ok(false);
        }
        self.record_once(
            loop_subject,
            "loop.state",
            BTreeMap::from([
                ("status".into(), Value::String("completed".into())),
                ("round".into(), Value::from(items.len() as u64)),
                ("items".into(), Value::Array(items)),
            ]),
        )?;
        self.store.set_step_state(&view.subject, "completed", None)
    }

    fn evaluate_candidate_loop(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        base_variables: &BTreeMap<String, String>,
    ) -> Result<bool> {
        let candidates = loop_spec
            .candidates
            .as_ref()
            .expect("the caller selected a candidate loop");
        let mut active = 0_u32;
        let mut completed = Vec::new();
        for candidate in 1..=candidates.count {
            if let Some(result) =
                self.loop_result_claim(loop_subject, view.attempt, Some(candidate))?
            {
                if result
                    .body
                    .pointer("/fields/status")
                    .and_then(Value::as_str)
                    != Some("failed")
                {
                    completed.push((candidate, result));
                }
                continue;
            }
            let dispatch =
                self.loop_dispatch_count(loop_subject, view.attempt, Some(candidate), None)?;
            let base_key = format!(
                "loop-candidate:{}:{}:{candidate}",
                view.subject, view.attempt
            );
            let key = if dispatch == 0 {
                base_key
            } else {
                format!("{base_key}:dispatch-{dispatch}")
            };
            let subject = self.store.mission_run_subject_for_idempotency_key(&key);
            if let Some(child) = self.store.mission_run(&subject)? {
                match child.status.as_str() {
                    "running" | "standing" | "blocked" => active += 1,
                    "failed" | "cancelled" => {
                        if self.loop_child_timed_out_without_claim(&child)? {
                            return self.reschedule_unclaimed_loop_child(
                                loop_subject,
                                view.attempt,
                                dispatch,
                                &child,
                                Some(candidate),
                                None,
                            );
                        }
                        let structural = self.loop_child_failure_is_structural(&child)?;
                        let reason = if structural {
                            "the candidate mission had a structural failure"
                        } else {
                            "the candidate mission failed"
                        };
                        self.record_loop_round(
                            loop_subject,
                            view.attempt,
                            &child,
                            "failed",
                            &BTreeMap::new(),
                            None,
                            Some(candidate),
                            None,
                            self.mission_run_token_usage(&child)?,
                            Some(reason),
                        )?;
                        if structural {
                            self.record_once(
                                loop_subject,
                                "loop.state",
                                BTreeMap::from([
                                    ("status".into(), Value::String("failed".into())),
                                    ("round".into(), Value::from(view.attempt)),
                                    ("reason".into(), Value::String(reason.into())),
                                ]),
                            )?;
                            return self.store.set_step_state(
                                &view.subject,
                                "cancelled",
                                Some(reason),
                            );
                        }
                    }
                    "completed" => {
                        let mut variables = base_variables.clone();
                        variables.insert("ST_CANDIDATE_INDEX".into(), candidate.to_string());
                        variables.insert("candidate.index".into(), candidate.to_string());
                        let metrics = match self.evaluate_loop_metrics(
                            run,
                            step,
                            view,
                            loop_spec,
                            &format!(
                                "{loop_subject}/round/{}/candidate/{candidate}",
                                view.attempt
                            ),
                            &variables,
                        ) {
                            Ok(Some(metrics)) => metrics,
                            Ok(None) => return Ok(false),
                            Err(error) => {
                                return self.store.set_step_state(
                                    &view.subject,
                                    "cancelled",
                                    Some(&format!("the candidate metric failed: {error:#}")),
                                );
                            }
                        };
                        let feedback = self.write_loop_feedback(
                            loop_subject,
                            view.attempt,
                            &child,
                            &metrics,
                            true,
                            Some(candidate),
                        )?;
                        self.record_loop_round(
                            loop_subject,
                            view.attempt,
                            &child,
                            "completed",
                            &metrics,
                            Some(&feedback),
                            Some(candidate),
                            None,
                            self.mission_run_token_usage(&child)?,
                            None,
                        )?;
                    }
                    _ => {}
                }
                continue;
            }
            if active < candidates.max_parallel {
                self.ensure_loop_child(
                    run,
                    view,
                    &loop_spec.round,
                    key,
                    view.attempt,
                    "",
                    None,
                    Some(candidate),
                )?;
                active += 1;
            }
        }
        let all_results = self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?
            .into_iter()
            .filter(|claim| {
                claim.body.pointer("/fields/round").and_then(Value::as_u64)
                    == Some(u64::from(view.attempt))
                    && claim.body.pointer("/fields/candidate").is_some()
            })
            .collect::<Vec<_>>();
        if all_results.len() < candidates.count as usize {
            return Ok(false);
        }
        completed = all_results
            .iter()
            .filter(|&claim| {
                claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("completed")
            })
            .cloned()
            .filter_map(|claim| {
                let candidate = claim
                    .body
                    .pointer("/fields/candidate")
                    .and_then(Value::as_u64)? as u32;
                Some((candidate, claim))
            })
            .collect();
        if completed.is_empty() {
            if let Some(child) = self.loop_result_child(&all_results)? {
                self.record_loop_round(
                    loop_subject,
                    view.attempt,
                    &child,
                    "failed",
                    &BTreeMap::new(),
                    None,
                    None,
                    None,
                    0,
                    Some("all candidate missions failed"),
                )?;
            }
            if self.repeated_loop_failures(loop_subject)?
                >= loop_spec.stop.repeated_failure.unwrap_or(u32::MAX)
            {
                return self.finish_exhausted_loop(
                    run,
                    view,
                    loop_spec,
                    loop_subject,
                    base_variables,
                    "the repeated-failure limit was reached",
                );
            }
            if view.attempt >= loop_spec.max_rounds {
                return self.finish_exhausted_loop(
                    run,
                    view,
                    loop_spec,
                    loop_subject,
                    base_variables,
                    "all candidate missions failed",
                );
            }
            return self.store.set_step_state(
                &view.subject,
                "failed",
                Some("all candidate missions failed"),
            );
        }
        let selection = self.select_loop_candidate(
            run,
            view,
            loop_spec,
            loop_subject,
            base_variables,
            &completed,
        )?;
        let winner = match selection {
            LoopCandidateSelection::Winner(winner) => winner,
            LoopCandidateSelection::Pending => return Ok(false),
            LoopCandidateSelection::NoWinner => {
                if let Some(child) = self.loop_result_child(
                    &completed
                        .iter()
                        .map(|(_, claim)| claim.clone())
                        .collect::<Vec<_>>(),
                )? {
                    self.record_loop_round(
                        loop_subject,
                        view.attempt,
                        &child,
                        "failed",
                        &BTreeMap::new(),
                        None,
                        None,
                        None,
                        0,
                        Some("the candidate selector rejected every candidate"),
                    )?;
                }
                if view.attempt >= loop_spec.max_rounds {
                    return self.finish_exhausted_loop(
                        run,
                        view,
                        loop_spec,
                        loop_subject,
                        base_variables,
                        "the candidate selector rejected every candidate",
                    );
                }
                return self.store.set_step_state(
                    &view.subject,
                    "failed",
                    Some("the candidate selector rejected every candidate"),
                );
            }
        };
        for (candidate, result) in &completed {
            let feedback = result
                .body
                .pointer("/fields/feedback")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match self.evaluate_loop_branch(
                run,
                view,
                loop_spec,
                *candidate == winner,
                feedback,
                Some(*candidate),
                None,
            )? {
                LoopBranchOutcome::Pending => return Ok(false),
                LoopBranchOutcome::Failed(reason) => {
                    return self
                        .store
                        .set_step_state(&view.subject, "cancelled", Some(&reason));
                }
                LoopBranchOutcome::Completed => {}
            }
        }
        let winner_result = completed
            .iter()
            .find(|(candidate, _)| *candidate == winner)
            .map(|(_, claim)| claim)
            .expect("the selected candidate exists");
        let best_metrics = winner_result
            .body
            .pointer("/fields/metrics")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        let winner_feedback = winner_result
            .body
            .pointer("/fields/feedback")
            .and_then(Value::as_str);
        let winner_child = self
            .loop_result_child(std::slice::from_ref(winner_result))?
            .context("the selected candidate mission disappeared")?;
        let winner_metrics = best_metrics
            .as_object()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(name, value)| value.as_f64().map(|value| (name.clone(), value)))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        self.record_loop_round(
            loop_subject,
            view.attempt,
            &winner_child,
            "completed",
            &winner_metrics,
            winner_feedback,
            None,
            None,
            0,
            Some(&format!("candidate {winner} was selected")),
        )?;
        let mut winner_variables = base_variables.clone();
        winner_variables.insert("ST_CANDIDATE_INDEX".into(), winner.to_string());
        winner_variables.insert("candidate.index".into(), winner.to_string());
        let mut passed = true;
        for gate in &loop_spec.until {
            match self.evaluate_context_gate(
                run,
                loop_subject,
                &loop_spec.id,
                &view.definition_hash,
                view.attempt,
                gate,
                &winner_variables,
            )? {
                GateOutcome::Pass => {}
                GateOutcome::Pending
                    if matches!(
                        gate,
                        GateSpec::Mechanical { .. } | GateSpec::Llm { .. } | GateSpec::Human { .. }
                    ) =>
                {
                    return Ok(false);
                }
                GateOutcome::Pending | GateOutcome::Fail(_) => passed = false,
            }
        }
        self.record_once(
            loop_subject,
            "loop.state",
            BTreeMap::from([
                (
                    "status".into(),
                    Value::String(if passed { "completed" } else { "running" }.into()),
                ),
                ("round".into(), Value::from(view.attempt)),
                ("winner".into(), Value::from(winner)),
                ("best_metrics".into(), best_metrics),
            ]),
        )?;
        if passed {
            return self.store.set_step_state(&view.subject, "completed", None);
        }
        if let Some(reason) = self.loop_stop_reason(loop_subject, loop_spec)? {
            return self.finish_exhausted_loop(
                run,
                view,
                loop_spec,
                loop_subject,
                &winner_variables,
                &reason,
            );
        }
        if view.attempt >= loop_spec.max_rounds {
            return self.finish_exhausted_loop(
                run,
                view,
                loop_spec,
                loop_subject,
                &winner_variables,
                "the candidate rounds did not satisfy the exit gates",
            );
        }
        self.store.set_step_state(
            &view.subject,
            "failed",
            Some("the selected candidate did not satisfy the exit gates"),
        )
    }

    fn select_loop_candidate(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        base_variables: &BTreeMap<String, String>,
        results: &[(u32, crate::model::ClaimRecord)],
    ) -> Result<LoopCandidateSelection> {
        let candidates = loop_spec
            .candidates
            .as_ref()
            .expect("the caller selected candidates");
        match &candidates.select {
            LoopCandidateSelector::Metric { metric } => {
                let spec = loop_spec
                    .metrics
                    .iter()
                    .find(|spec| &spec.name == metric)
                    .context("the candidate metric disappeared")?;
                Ok(results
                    .iter()
                    .filter_map(|(candidate, claim)| {
                        claim
                            .body
                            .pointer(&format!("/fields/metrics/{metric}"))
                            .and_then(Value::as_f64)
                            .map(|value| (*candidate, value))
                    })
                    .max_by(|left, right| {
                        let order = left.1.total_cmp(&right.1);
                        if spec.direction == "higher" {
                            order
                        } else {
                            order.reverse()
                        }
                    })
                    .map_or(LoopCandidateSelection::NoWinner, |(candidate, _)| {
                        LoopCandidateSelection::Winner(candidate)
                    }))
            }
            LoopCandidateSelector::Llm { gate } | LoopCandidateSelector::Human { gate } => {
                for (candidate, _) in results {
                    let mut variables = base_variables.clone();
                    variables.insert("ST_CANDIDATE_INDEX".into(), candidate.to_string());
                    variables.insert("candidate.index".into(), candidate.to_string());
                    match self.evaluate_context_gate(
                        run,
                        &format!(
                            "{loop_subject}/round/{}/candidate/{candidate}",
                            view.attempt
                        ),
                        &format!("select candidate {candidate}"),
                        &view.definition_hash,
                        view.attempt,
                        gate,
                        &variables,
                    )? {
                        GateOutcome::Pass => {
                            return Ok(LoopCandidateSelection::Winner(*candidate));
                        }
                        GateOutcome::Pending => return Ok(LoopCandidateSelection::Pending),
                        GateOutcome::Fail(_) => {}
                    }
                }
                Ok(LoopCandidateSelection::NoWinner)
            }
        }
    }

    fn loop_result_child(
        &self,
        results: &[crate::model::ClaimRecord],
    ) -> Result<Option<MissionRunView>> {
        let Some(subject) = results.iter().find_map(|claim| {
            claim
                .body
                .pointer("/fields/mission_run")
                .and_then(Value::as_str)
        }) else {
            return Ok(None);
        };
        self.store.mission_run(subject)
    }

    fn loop_first_execution_at(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
    ) -> Result<Option<u128>> {
        let root = run
            .root_mission_run
            .strip_prefix("mission-run/")
            .unwrap_or(&run.root_mission_run);
        let mut started = None;
        for child in self
            .store
            .mission_runs_for_root(root)?
            .into_iter()
            .filter(|child| child.parent_step_run.as_deref() == Some(view.subject.as_str()))
        {
            let has_claimable_work = child.steps.iter().any(|step| !step.agentless);
            if !has_claimable_work {
                started = Some(started.map_or(child.created_at_unix_ms, |current: u128| {
                    current.min(child.created_at_unix_ms)
                }));
                continue;
            }
            for step in &child.steps {
                for claim in self.store.claims_for(&step.subject, Some("work.claimed"))? {
                    started = Some(started.map_or(claim.accepted_at_unix_ms, |current: u128| {
                        current.min(claim.accepted_at_unix_ms)
                    }));
                }
            }
        }
        Ok(started)
    }

    fn loop_child_timed_out_without_claim(&self, child: &MissionRunView) -> Result<bool> {
        if !child.steps.iter().any(|step| !step.agentless) {
            return Ok(false);
        }
        let timed_out = self
            .store
            .claims_for(&child.subject, Some("mission-run.state"))?
            .iter()
            .any(|claim| {
                claim
                    .body
                    .pointer("/fields/reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.contains("timeout expired"))
            });
        if !timed_out {
            return Ok(false);
        }
        for step in &child.steps {
            if !self
                .store
                .claims_for(&step.subject, Some("work.claimed"))?
                .is_empty()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn loop_dispatch_count(
        &self,
        loop_subject: &str,
        round: u32,
        candidate: Option<u32>,
        item_id: Option<&str>,
    ) -> Result<u32> {
        Ok(self
            .store
            .claims_for(loop_subject, Some("loop.round-dispatch"))?
            .into_iter()
            .filter(|claim| {
                let fields = claim.body.get("fields").unwrap_or(&claim.body);
                fields.get("round").and_then(Value::as_u64) == Some(u64::from(round))
                    && fields.get("candidate").and_then(Value::as_u64) == candidate.map(u64::from)
                    && fields.get("item_id").and_then(Value::as_str) == item_id
            })
            .filter_map(|claim| {
                claim
                    .body
                    .pointer("/fields/dispatch")
                    .and_then(Value::as_u64)
                    .and_then(|dispatch| u32::try_from(dispatch).ok())
            })
            .max()
            .unwrap_or(0))
    }

    #[allow(clippy::too_many_arguments)]
    fn reschedule_unclaimed_loop_child(
        &self,
        loop_subject: &str,
        round: u32,
        dispatch: u32,
        child: &MissionRunView,
        candidate: Option<u32>,
        item_id: Option<&str>,
    ) -> Result<bool> {
        let next_dispatch = dispatch.saturating_add(1);
        let reason = "the round mission timeout expired before any worker claimed its work; rescheduled without consuming max-rounds";
        let mut fields = BTreeMap::from([
            ("round".into(), Value::from(round)),
            ("dispatch".into(), Value::from(next_dispatch)),
            ("status".into(), Value::String("rescheduled".into())),
            ("mission_run".into(), Value::String(child.subject.clone())),
            ("reason".into(), Value::String(reason.into())),
        ]);
        if let Some(candidate) = candidate {
            fields.insert("candidate".into(), Value::from(candidate));
        }
        if let Some(item_id) = item_id {
            fields.insert("item_id".into(), Value::String(item_id.into()));
        }
        self.store.append_claim(&ClaimInput {
            subject: loop_subject.into(),
            kind: "loop.round-dispatch".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "loop-round-dispatch:{loop_subject}:{round}:{next_dispatch}:{}:{}",
                candidate.map_or_else(|| "round".into(), |value| value.to_string()),
                item_id.unwrap_or("none")
            )),
        })?;
        self.record_once(
            loop_subject,
            "loop.state",
            BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("round".into(), Value::from(round)),
                ("reason".into(), Value::String(reason.into())),
            ]),
        )?;
        self.signal_changed();
        Ok(true)
    }

    fn loop_child_failure_is_structural(&self, child: &MissionRunView) -> Result<bool> {
        if child.status == "cancelled" || child.steps.iter().any(|step| step.status == "cancelled")
        {
            return Ok(true);
        }
        Ok(self
            .store
            .latest_claim(&child.subject, Some("mission-run.state"))?
            .and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .is_some_and(|reason| {
                reason.contains("revision is unavailable")
                    || reason.contains("structural")
                    || reason.contains("invalid")
            }))
    }

    #[allow(clippy::too_many_arguments)]
    fn record_loop_round(
        &self,
        loop_subject: &str,
        round: u32,
        child: &MissionRunView,
        status: &str,
        metrics: &BTreeMap<String, f64>,
        feedback: Option<&str>,
        candidate: Option<u32>,
        item: Option<&Value>,
        token_usage: u64,
        reason: Option<&str>,
    ) -> Result<()> {
        let mut fields = BTreeMap::from([
            ("round".into(), Value::from(round)),
            ("status".into(), Value::String(status.into())),
            ("mission_run".into(), Value::String(child.subject.clone())),
            ("metrics".into(), serde_json::to_value(metrics)?),
            ("token_usage".into(), Value::from(token_usage)),
        ]);
        if let Some(feedback) = feedback {
            fields.insert("feedback".into(), Value::String(feedback.into()));
        }
        if let Some(candidate) = candidate {
            fields.insert("candidate".into(), Value::from(candidate));
        }
        if let Some(item) = item {
            fields.insert("item".into(), item.clone());
        }
        if let Some(reason) = reason {
            fields.insert("reason".into(), Value::String(reason.into()));
        }
        self.store.append_claim(&ClaimInput {
            subject: loop_subject.into(),
            kind: "loop.round-result".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "loop-round-result:{loop_subject}:{round}:{}",
                candidate.map_or_else(|| "round".into(), |value| format!("candidate-{value}"))
            )),
        })?;
        self.signal_changed();
        Ok(())
    }

    fn repeated_loop_failures(&self, loop_subject: &str) -> Result<u32> {
        Ok(self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?
            .iter()
            .rev()
            .filter(|claim| claim.body.pointer("/fields/candidate").is_none())
            .filter(|claim| claim.body.pointer("/fields/item").is_none())
            .take_while(|claim| {
                claim.body.pointer("/fields/status").and_then(Value::as_str) == Some("failed")
            })
            .count() as u32)
    }

    fn mission_run_token_usage(&self, run: &MissionRunView) -> Result<u64> {
        let desired = self.store.desired_subjects()?;
        desired
            .iter()
            .filter(|subject| subject.owner_run.as_deref() == Some(run.subject.as_str()))
            .try_fold(0_u64, |total, subject| {
                Ok(total.saturating_add(
                    self.store
                        .usage_summary_at(&subject.subject, None, None)?
                        .map(|usage| usage.total_tokens)
                        .unwrap_or_default(),
                ))
            })
    }

    fn loop_best(
        &self,
        loop_subject: &str,
        loop_spec: &LoopSpec,
    ) -> Result<(u64, BTreeMap<String, f64>)> {
        let claims = self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?;
        let selected = if let Some(metric_name) = &loop_spec.keep_metric {
            let metric = loop_spec
                .metrics
                .iter()
                .find(|metric| &metric.name == metric_name)
                .context("the keep metric disappeared")?;
            claims
                .iter()
                .filter(|claim| claim.body.pointer("/fields/candidate").is_none())
                .filter(|claim| claim.body.pointer("/fields/item").is_none())
                .filter(|claim| {
                    claim.body.pointer("/fields/status").and_then(Value::as_str)
                        == Some("completed")
                })
                .filter_map(|claim| {
                    let round = claim.body.pointer("/fields/round")?.as_u64()?;
                    let value = claim
                        .body
                        .pointer(&format!("/fields/metrics/{metric_name}"))?
                        .as_f64()?;
                    Some((claim, round, value))
                })
                .max_by(|left, right| {
                    let order = left.2.total_cmp(&right.2);
                    if metric.direction == "higher" {
                        order
                    } else {
                        order.reverse()
                    }
                })
                .map(|(claim, round, _)| (claim, round))
        } else {
            claims
                .iter()
                .rfind(|claim| {
                    claim.body.pointer("/fields/candidate").is_none()
                        && claim.body.pointer("/fields/item").is_none()
                        && claim.body.pointer("/fields/status").and_then(Value::as_str)
                            == Some("completed")
                })
                .and_then(|claim| {
                    claim
                        .body
                        .pointer("/fields/round")
                        .and_then(Value::as_u64)
                        .map(|round| (claim, round))
                })
        };
        let Some((claim, round)) = selected else {
            return Ok((0, BTreeMap::new()));
        };
        let metrics = claim
            .body
            .pointer("/fields/metrics")
            .and_then(Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(name, value)| value.as_f64().map(|value| (name.clone(), value)))
                    .collect()
            })
            .unwrap_or_default();
        Ok((round, metrics))
    }

    fn loop_stop_reason(&self, loop_subject: &str, loop_spec: &LoopSpec) -> Result<Option<String>> {
        let results = self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?;
        if let Some(limit) = loop_spec.stop.token_budget {
            let used = results
                .iter()
                .filter_map(|claim| {
                    claim
                        .body
                        .pointer("/fields/token_usage")
                        .and_then(Value::as_u64)
                })
                .sum::<u64>();
            if used >= limit {
                return Ok(Some(format!(
                    "the loop token budget of {limit} was reached"
                )));
            }
        }
        if let (Some(metric_name), Some(rounds)) = (
            &loop_spec.stop.plateau_metric,
            loop_spec.stop.plateau_rounds,
        ) {
            let metric = loop_spec
                .metrics
                .iter()
                .find(|metric| &metric.name == metric_name)
                .context("the plateau metric disappeared")?;
            let mut best: Option<f64> = None;
            let mut stalled = 0_u32;
            for value in results
                .iter()
                .filter(|claim| claim.body.pointer("/fields/candidate").is_none())
                .filter(|claim| claim.body.pointer("/fields/item").is_none())
                .filter_map(|claim| {
                    claim
                        .body
                        .pointer(&format!("/fields/metrics/{metric_name}"))
                        .and_then(Value::as_f64)
                })
            {
                let improves = best.is_none_or(|prior| {
                    if metric.direction == "higher" {
                        value - prior >= metric.min_improvement
                    } else {
                        prior - value >= metric.min_improvement
                    }
                });
                if improves {
                    best = Some(value);
                    stalled = 0;
                } else {
                    stalled += 1;
                }
            }
            if stalled >= rounds {
                return Ok(Some(format!(
                    "the loop metric did not improve for {rounds} rounds"
                )));
            }
        }
        Ok(None)
    }

    fn finish_exhausted_loop(
        &self,
        run: &MissionRunView,
        view: &crate::model::StepRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        variables: &BTreeMap<String, String>,
        reason: &str,
    ) -> Result<bool> {
        match &loop_spec.on_exhausted {
            LoopExhaustionSpec::Fail => {
                self.request_loop_exhaustion_attention(run, loop_spec, loop_subject, reason)?;
                self.record_once(
                    loop_subject,
                    "loop.state",
                    BTreeMap::from([
                        ("status".into(), Value::String("failed".into())),
                        ("round".into(), Value::from(view.attempt)),
                        ("reason".into(), Value::String(reason.into())),
                    ]),
                )?;
                self.store
                    .set_step_state(&view.subject, "failed", Some(reason))
            }
            LoopExhaustionSpec::Succeed => {
                self.record_once(
                    loop_subject,
                    "loop.state",
                    BTreeMap::from([
                        ("status".into(), Value::String("exhausted".into())),
                        ("round".into(), Value::from(view.attempt)),
                        ("reason".into(), Value::String(reason.into())),
                    ]),
                )?;
                self.store.set_step_state(
                    &view.subject,
                    "completed",
                    Some("the loop accepted its best result at exhaustion"),
                )
            }
            LoopExhaustionSpec::Human { gate } => match self.evaluate_context_gate(
                run,
                loop_subject,
                &loop_spec.id,
                &view.definition_hash,
                view.attempt,
                gate,
                variables,
            )? {
                GateOutcome::Pass => {
                    self.record_once(
                        loop_subject,
                        "loop.state",
                        BTreeMap::from([
                            ("status".into(), Value::String("exhausted".into())),
                            ("round".into(), Value::from(view.attempt)),
                            ("reason".into(), Value::String(reason.into())),
                        ]),
                    )?;
                    self.store.set_step_state(
                        &view.subject,
                        "completed",
                        Some("the human reviewer accepted the exhausted loop result"),
                    )
                }
                GateOutcome::Fail(review_reason) => {
                    self.record_once(
                        loop_subject,
                        "loop.state",
                        BTreeMap::from([
                            ("status".into(), Value::String("failed".into())),
                            ("round".into(), Value::from(view.attempt)),
                            ("reason".into(), Value::String(review_reason.clone())),
                        ]),
                    )?;
                    self.store
                        .set_step_state(&view.subject, "failed", Some(&review_reason))
                }
                GateOutcome::Pending => Ok(false),
            },
        }
    }

    fn request_loop_exhaustion_attention(
        &self,
        run: &MissionRunView,
        loop_spec: &LoopSpec,
        loop_subject: &str,
        reason: &str,
    ) -> Result<()> {
        let Some(attention) = &loop_spec.exhaustion_attention else {
            return Ok(());
        };
        let feedback = self
            .store
            .claims_for(loop_subject, Some("loop.round-result"))?
            .into_iter()
            .rev()
            .find_map(|claim| {
                claim
                    .body
                    .pointer("/fields/feedback")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        let detail = feedback.as_deref().map_or_else(
            || format!("Loop `{loop_subject}` failed: {reason}."),
            |feedback| {
                format!("Loop `{loop_subject}` failed: {reason}. Latest feedback: `{feedback}`.")
            },
        );
        let idempotency_key = format!("loop-exhausted-attention:{loop_subject}");
        let digest = hex::encode(sha2::Sha256::digest(idempotency_key.as_bytes()));
        self.store.request_attention(
            &format!("attention/{}", &digest[..32]),
            &AttentionRequest {
                reviewer: attention.reviewer.clone(),
                title: attention.title.clone(),
                reason: detail,
                severity: attention.severity.clone(),
                targets: vec![loop_subject.into(), run.subject.clone()],
                actor: "agent/st3/reconciler".into(),
                idempotency_key,
            },
        )?;
        self.signal_changed();
        Ok(())
    }

    fn step_dependencies_hold(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        views: &HashMap<&str, &crate::model::StepRunView>,
    ) -> Result<bool> {
        if let Some(parent) = &step.parent {
            let Some(parent) = views.get(parent.as_str()) else {
                return Ok(false);
            };
            if !matches!(parent.status.as_str(), "claimed" | "working" | "verifying") {
                return Ok(false);
            }
        }
        for dependency in &step.spec.dependencies {
            match dependency {
                DependencySpec::Step {
                    step: target,
                    state,
                } => {
                    let path = if step.dependency_prefix.is_empty() {
                        target.clone()
                    } else {
                        format!("{}/{target}", step.dependency_prefix)
                    };
                    let Some(target) = views.get(path.as_str()) else {
                        return Ok(false);
                    };
                    let holds = match state.as_str() {
                        "completed" => target.status == "completed",
                        "failed" => target.status == "failed",
                        "terminal" => {
                            matches!(target.status.as_str(), "completed" | "failed" | "cancelled")
                        }
                        _ => false,
                    };
                    if !holds {
                        return Ok(false);
                    }
                }
                DependencySpec::Predicate { gate } => {
                    let fake = crate::model::StepRunView {
                        subject: format!(
                            "step-run/{}/{}",
                            run.generation
                                .strip_prefix("run-generation/")
                                .unwrap_or(&run.generation),
                            step.spec.path
                        ),
                        run: run.subject.clone(),
                        generation: run.generation.clone(),
                        step: step.spec.path.clone(),
                        fresh_context: step.spec.fresh_context,
                        queue: None,
                        queue_position: None,
                        definition_hash: step.spec.definition_hash.clone(),
                        status: "pending".into(),
                        attempt: 1,
                        assigned_to: None,
                        available_to: Vec::new(),
                        agentless: true,
                        title: None,
                        goals: Vec::new(),
                        constraints: Vec::new(),
                        under: Vec::new(),
                        worker_reported: false,
                        claimant: None,
                        claim_incarnation: None,
                        claim_expires_at_unix_ms: None,
                        carried_claimant: None,
                        execution_started_at_unix_ms: None,
                        execution_elapsed_ms: 0,
                        timeout_ms: None,
                        ready_age_ms: None,
                        wake: None,
                        progress_summary: None,
                        progress_at_unix_ms: None,
                        completion_summary: None,
                        readiness_epoch: 0,
                        blocked_reason: None,
                        blockers: Vec::new(),
                        not_before_unix_ms: None,
                        created_at_unix_ms: run.created_at_unix_ms,
                        updated_at_unix_ms: run.updated_at_unix_ms,
                    };
                    if !matches!(
                        self.evaluate_mission_gate(run, step, &fake, gate)?,
                        GateOutcome::Pass
                    ) {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    fn materialize_step_declarations(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        assigned_agents_only: bool,
    ) -> Result<bool> {
        let Some(source) = &step.spec.declarations_kdl else {
            return Ok(false);
        };
        let variables = run_variables(run, step, view);
        let source = crate::mission::interpolate_kdl(source, &variables)?;
        let mut intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
        if assigned_agents_only {
            intent.subjects.retain(|subject, desired| {
                desired.kind == "agent"
                    && (view.assigned_to.as_deref() == Some(subject.as_str())
                        || view.available_to.iter().any(|agent| agent == subject))
            });
            if intent.subjects.is_empty() {
                return Ok(false);
            }
        }
        for subject in intent.subjects.values_mut() {
            subject.owner_run = Some(run.subject.clone());
            subject.owner_generation = Some(run.generation.clone());
            subject.owner_step = Some(view.subject.clone());
            if let Some(member) = subject.member.as_mut() {
                let workspace = PathBuf::from(&member.workspace);
                if workspace.is_relative() {
                    member.workspace = Path::new(&run.workspace)
                        .join(&workspace)
                        .to_string_lossy()
                        .into_owned();
                }
                let cwd = PathBuf::from(&member.cwd);
                if cwd.is_relative() {
                    member.cwd = Path::new(&run.workspace)
                        .join(&cwd)
                        .to_string_lossy()
                        .into_owned();
                }
                member.environment.extend(variables.clone());
                member
                    .environment
                    .insert("ST3_RUN_DIR".into(), run.workspace.clone());
                member
                    .environment
                    .insert("ST3_ENDPOINT".into(), self.endpoint.clone());
            }
        }
        self.reject_runtime_collisions(&intent, run, Some(&view.subject))?;
        let response = self.store.apply_internal(
            &intent,
            &format!("materialize:{}:{}", view.subject, view.attempt),
        )?;
        Ok(response.changed)
    }

    /// Retire the declarations an earlier generation materialized. The second value is false
    /// while an active step still holds some of them, so a later pass retires them once the
    /// step leaves without re-owning them.
    fn retire_predecessor_generation(&self, run: &MissionRunView) -> Result<(bool, bool)> {
        // An active step re-owns its own declarations when it next materializes them, so a
        // claim carried into this generation keeps its step's members.
        let active_steps = run
            .steps
            .iter()
            .filter(|step| {
                matches!(
                    step.status.as_str(),
                    "ready" | "claimed" | "working" | "verifying" | "blocked"
                )
            })
            .map(|step| step.step.as_str())
            .collect::<BTreeSet<_>>();
        let (held, retired): (Vec<_>, Vec<_>) = self
            .store
            .desired_subjects_for_owner_run(&run.subject)?
            .into_iter()
            .filter(|subject| subject.owner_run.as_deref() == Some(run.subject.as_str()))
            // Only a subject materialized by another generation is retired. An
            // eval owns its top-level seats for the whole run, not one generation.
            .filter(|subject| {
                subject
                    .owner_generation
                    .as_deref()
                    .is_some_and(|generation| generation != run.generation)
            })
            .partition(|subject| {
                subject
                    .owner_step
                    .as_deref()
                    .and_then(step_path_of_subject)
                    .is_some_and(|path| active_steps.contains(path))
            });
        let complete = held.is_empty();
        let mut changed = self.stop_owned_intake(
            &retired.iter().collect::<Vec<_>>(),
            Some(&run.generation),
            &format!("retire-generation-intake:{}", run.generation),
        )?;
        let stops = retired
            .iter()
            .filter(|subject| subject.member.is_some() && subject.kind != "stop")
            .map(|subject| format!("stop {:?}", subject.subject))
            .collect::<Vec<_>>()
            .join("\n");
        if stops.is_empty() {
            return Ok((changed, complete));
        }
        let source = format!("version 2\n\n{stops}\n");
        let mut intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
        for subject in intent.subjects.values_mut() {
            subject.owner_generation = Some(run.generation.clone());
        }
        let response = self
            .store
            .apply_internal(&intent, &format!("retire-generation:{}", run.generation))?;
        changed |= response.changed;
        Ok((changed, complete))
    }

    /// Stop owned observers, subscriptions, and schedules durably. Each one remains a stopped
    /// declaration of its own kind, so it settles its state and starts no more work.
    fn stop_owned_intake(
        &self,
        subjects: &[&DesiredSubject],
        generation: Option<&str>,
        key: &str,
    ) -> Result<bool> {
        let mut by_owner = BTreeMap::<&str, Vec<String>>::new();
        for subject in subjects {
            if !matches!(
                subject.kind.as_str(),
                "observer" | "subscription" | "schedule"
            ) || intake_is_stopped(subject, &self.host)
            {
                continue;
            }
            let Some(owner) = subject.owner_run.as_deref() else {
                continue;
            };
            let owner_id = owner.strip_prefix("mission-run/").unwrap_or(owner);
            let Some(name) = subject
                .subject
                .strip_prefix(&format!("{}/{owner_id}/", subject.kind))
            else {
                continue;
            };
            by_owner
                .entry(owner_id)
                .or_default()
                .push(format!("{} {name:?} {{ stop }}", subject.kind));
        }
        let mut changed = false;
        for (owner_id, stops) in by_owner {
            let source = format!("version 2\n\n{}\n", stops.join("\n"));
            let mut intent = crate::graph::parse_execution_intent(&source, &self.host, owner_id)?;
            for subject in intent.subjects.values_mut() {
                subject.owner_generation = generation.map(str::to_owned);
            }
            changed |= self
                .store
                .apply_internal(&intent, &format!("{key}:{owner_id}"))?
                .changed;
        }
        Ok(changed)
    }

    fn materialize_mission_declarations(
        &self,
        run: &MissionRunView,
        mission: &MissionSpec,
    ) -> Result<bool> {
        let Some(source) = &mission.declarations_kdl else {
            return Ok(false);
        };
        let variables = crate::store::mission_run_variables(run, &mission.revision);
        let source = crate::mission::interpolate_kdl(source, &variables)?;
        #[cfg(test)]
        self.mission_declaration_parses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
        let selected = self
            .store
            .desired_subjects_for_owner_run(&run.subject)?
            .into_iter()
            .map(|subject| (subject.subject.clone(), subject))
            .collect::<BTreeMap<_, _>>();
        // A stop declared by a step is authoritative for the remainder of its generation.
        // Re-materializing the mission-level agent on the next final-phase pass would otherwise
        // replace that stop for one reconciliation cycle, launch a fresh incarnation, and then
        // stop it again during terminal cleanup.
        intent.subjects.retain(|subject, _| {
            !selected.get(subject).is_some_and(|current| {
                current.kind == "stop"
                    && current.owner_run.as_deref() == Some(run.subject.as_str())
                    && current.owner_generation.as_deref() == Some(run.generation.as_str())
            })
        });
        for subject in intent.subjects.values_mut() {
            subject.owner_run = Some(run.subject.clone());
            subject.owner_generation = Some(run.generation.clone());
            subject.owner_step = None;
            if let Some(member) = subject.member.as_mut() {
                let workspace = PathBuf::from(&member.workspace);
                if workspace.is_relative() {
                    member.workspace = Path::new(&run.workspace)
                        .join(&workspace)
                        .to_string_lossy()
                        .into_owned();
                }
                let cwd = PathBuf::from(&member.cwd);
                if cwd.is_relative() {
                    member.cwd = Path::new(&run.workspace)
                        .join(&cwd)
                        .to_string_lossy()
                        .into_owned();
                }
                member.environment.extend(variables.clone());
                member
                    .environment
                    .insert("ST3_RUN_DIR".into(), run.workspace.clone());
                member
                    .environment
                    .insert("ST3_ENDPOINT".into(), self.endpoint.clone());
            }
        }
        self.reject_runtime_collisions(&intent, run, None)?;
        let response = self
            .store
            .apply_internal(&intent, &format!("materialize:{}", run.generation))?;
        Ok(response.changed)
    }

    fn reject_runtime_collisions(
        &self,
        intent: &crate::model::NormalizedIntent,
        run: &MissionRunView,
        owner_step: Option<&str>,
    ) -> Result<()> {
        let existing = self
            .store
            .desired_subjects_for_owner_run(&run.subject)?
            .into_iter()
            .map(|subject| (subject.subject.clone(), subject))
            .collect::<BTreeMap<_, _>>();
        for subject in intent.subjects.values().filter(|subject| {
            matches!(
                subject.kind.as_str(),
                "agent" | "exec" | "pty" | "observer" | "subscription" | "schedule"
            )
        }) {
            let Some(current) = existing.get(&subject.subject) else {
                continue;
            };
            if current.kind != "stop"
                && current.owner_run.as_deref() == Some(run.subject.as_str())
                && current.owner_generation.as_deref() == Some(run.generation.as_str())
                && current.owner_step.as_deref() != owner_step
            {
                return Err(crate::model::St3Error::new(
                    "duplicate-runtime-subject",
                    format!(
                        "mission run `{}` declares runtime `{}` in more than one mission or step",
                        run.subject, subject.subject
                    ),
                )
                .into());
            }
        }
        Ok(())
    }

    fn step_declarations_hold(&self, step_subject: &str) -> Result<bool> {
        for subject in self
            .store
            .desired_subjects_for_owner_step(step_subject)?
            .into_iter()
            .filter(|subject| subject.owner_step.as_deref() == Some(step_subject))
        {
            let status = self
                .store
                .latest_actual_value(&subject.subject)?
                .as_ref()
                .and_then(|actual| actual_field(actual, "status"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let harness = self.store.current_harness(&subject.subject)?;
            let holds = match subject.kind.as_str() {
                "stop" => {
                    matches!(status.as_deref(), Some("stopped" | "absent" | "exited"))
                }
                "message" => matches!(status.as_deref(), Some("delivered" | "read" | "closed")),
                _ => match subject.member.as_ref() {
                    Some(member) if member.driver.is_some() => {
                        harness.as_ref().is_some_and(|harness| harness.is_ready())
                    }
                    Some(_) => matches!(
                        status.as_deref(),
                        Some("running" | "ready" | "working" | "idle" | "exited")
                    ),
                    None => true,
                },
            };
            if !holds {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Return the first produced native driver that cannot still satisfy this step.
    ///
    /// A driver with no restart policy has no path from a terminal runtime back to readiness. A
    /// restartable driver remains pending until its policy either succeeds or publishes an
    /// explicit `raise` decision. Without this check, a dead `restart "never"` agent leaves its
    /// producing step parked until the step timeout, hiding an immediate failure for minutes.
    fn step_declaration_failure(&self, step_subject: &str) -> Result<Option<String>> {
        for subject in self
            .store
            .desired_subjects_for_owner_step(step_subject)?
            .into_iter()
            .filter(|subject| subject.owner_step.as_deref() == Some(step_subject))
        {
            let Some(member) = subject
                .member
                .as_ref()
                .filter(|member| member.driver.is_some())
            else {
                continue;
            };
            let actual = self.store.latest_actual_value(&subject.subject)?;
            let status = actual
                .as_ref()
                .and_then(|actual| actual_field(actual, "status"))
                .and_then(Value::as_str);
            let harness = self.store.current_harness(&subject.subject)?;
            let harness_state = harness.as_ref().map(|harness| harness.state.as_str());
            if harness.as_ref().is_some_and(|harness| harness.is_ready()) {
                continue;
            }

            let raised = self
                .store
                .latest_claim(&subject.subject, Some("runtime.reconcile-decision"))?
                .filter(|claim| {
                    claim
                        .body
                        .pointer("/fields/decision")
                        .and_then(Value::as_str)
                        == Some("raise")
                });
            let terminal_without_restart = member.restart == RestartType::Never
                && (matches!(status, Some("absent" | "exited" | "vanished" | "stopped"))
                    || harness_state == Some("ended"));
            if !terminal_without_restart && raised.is_none() {
                continue;
            }

            let action_failure = self
                .store
                .latest_observation(&subject.subject, "runtime.action.failed")?
                .and_then(|claim| {
                    claim
                        .body
                        .pointer("/fields/reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                });
            let raised_reason = raised.and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let exit = actual
                .as_ref()
                .and_then(|actual| actual_field(actual, "exit_code"))
                .and_then(Value::as_i64)
                .map(|code| format!(" with exit code {code}"))
                .unwrap_or_default();
            let detail = action_failure.or(raised_reason).unwrap_or_else(|| {
                format!("runtime status was {}{exit}", status.unwrap_or("ended"))
            });
            return Ok(Some(format!(
                "required driver `{}` could not become ready: {detail}",
                subject.subject
            )));
        }
        Ok(None)
    }

    fn missing_product(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
    ) -> Result<Option<MissingProduct>> {
        let variables = run_variables(run, step, view);
        self.first_missing_product(&step.spec.products, &variables)
    }

    fn products_hold_with_variables(
        &self,
        products: &[crate::model::ProductSpec],
        variables: &BTreeMap<String, String>,
    ) -> Result<bool> {
        Ok(self.first_missing_product(products, variables)?.is_none())
    }

    fn first_missing_product(
        &self,
        products: &[crate::model::ProductSpec],
        variables: &BTreeMap<String, String>,
    ) -> Result<Option<MissingProduct>> {
        for product in products {
            let subject = crate::mission::interpolate(&product.subject, variables)?;
            let mut fields = Vec::with_capacity(product.fields.len());
            for (field, expected) in &product.fields {
                let expected = match expected {
                    Value::String(value) => {
                        Value::String(crate::mission::interpolate(value, variables)?)
                    }
                    value => value.clone(),
                };
                fields.push((field.clone(), expected));
            }
            let holds = self.subject_value(&subject)?.is_some_and(|actual| {
                fields
                    .iter()
                    .all(|(field, expected)| actual_field(&actual, field) == Some(expected))
            });
            if !holds {
                return Ok(Some(MissingProduct { subject, fields }));
            }
        }
        Ok(None)
    }

    /// A worker that submits before its declared product exists leaves the step verifying with no
    /// further prompt. Once that turn has ended, tell the worker exactly which subject and fields
    /// the step waits for. The message is sent once per step attempt and readiness epoch.
    fn notify_missing_product(
        &self,
        view: &crate::model::StepRunView,
        missing: &MissingProduct,
    ) -> Result<bool> {
        if view.agentless || !view.worker_reported || view.status != "verifying" {
            return Ok(false);
        }
        let Some(agent) = view.claimant.as_deref().or(view.assigned_to.as_deref()) else {
            return Ok(false);
        };
        if self
            .store
            .current_harness(agent)?
            .is_some_and(|harness| harness.state == "working")
        {
            return Ok(false);
        }
        let idempotency_key = format!(
            "product-wait:{}@{}@{}",
            view.subject, view.attempt, view.readiness_epoch
        );
        let message_id = &hex::encode(sha2::Sha256::digest(idempotency_key.as_bytes()))[..16];
        let message_subject = format!("message/{message_id}");
        if self
            .store
            .latest_claim(&message_subject, Some("message.sent"))?
            .is_some()
        {
            return Ok(false);
        }
        let expected = missing
            .fields
            .iter()
            .map(|(field, value)| match value {
                Value::String(value) => format!("{field}={value}"),
                value => format!("{field}={value}"),
            })
            .collect::<Vec<_>>();
        let example = expected
            .iter()
            .map(|field| format!(" --field {field}"))
            .collect::<String>();
        let content = format!(
            "`{}` was submitted, but its declared product `{}` has not been observed{}. Record that exact subject, for example `st claim {} resource.observed --actor {agent}{example}`, or fail the step with the reason. No action is needed if another actor produces it.",
            view.subject,
            missing.subject,
            if expected.is_empty() {
                String::new()
            } else {
                format!(" with {}", expected.join(", "))
            },
            missing.subject,
        );
        let title = format!(
            "Declared product missing: {}",
            view.title.as_deref().unwrap_or(&view.step)
        );
        self.store.append_claim(&ClaimInput {
            subject: message_subject,
            kind: "message.sent".into(),
            actor: Some("daemon/runtime".into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String("daemon/runtime".into())),
                ("to".into(), Value::String(agent.into())),
                ("content".into(), Value::String(content)),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String(title)),
                ("in_reply_to".into(), Value::Null),
                (
                    "tags".into(),
                    Value::Array(vec![
                        Value::String(format!("mission-run:{}", view.run)),
                        Value::String(format!("st3-product-wait:{}", view.subject)),
                    ]),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(idempotency_key),
        })?;
        Ok(true)
    }

    fn evaluate_mission_gate(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
        gate: &GateSpec,
    ) -> Result<GateOutcome> {
        let variables = run_variables(run, step, view);
        self.evaluate_context_gate(
            run,
            &view.subject,
            step.spec.title.as_deref().unwrap_or(&step.spec.path),
            &view.definition_hash,
            view.attempt,
            gate,
            &variables,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_context_gate(
        &self,
        run: &MissionRunView,
        subject: &str,
        title: &str,
        definition_hash: &str,
        attempt: u32,
        gate: &GateSpec,
        variables: &BTreeMap<String, String>,
    ) -> Result<GateOutcome> {
        let mut gate = gate.clone();
        expand_gate(&mut gate, variables, &run.workspace)?;
        if let GateSpec::Human {
            reviewer,
            mode,
            question,
            review_targets,
            ..
        } = &gate
        {
            return self.evaluate_mission_human_gate(
                run,
                subject,
                title,
                definition_hash,
                attempt,
                reviewer,
                mode.as_deref().unwrap_or("approve"),
                question.as_deref(),
                review_targets,
            );
        }
        let stage = GateContext {
            subject: subject.to_owned(),
            name: title.to_owned(),
            started_at_unix_ms: run
                .steps
                .iter()
                .find(|step| step.subject == subject)
                .map_or(run.created_at_unix_ms, |step| step.created_at_unix_ms),
            attempt,
        };
        self.evaluate_gate(&stage, &gate)
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_mission_human_gate(
        &self,
        run: &MissionRunView,
        subject: &str,
        title: &str,
        definition_hash: &str,
        attempt: u32,
        reviewer: &str,
        mode: &str,
        question: Option<&str>,
        review_targets: &[String],
    ) -> Result<GateOutcome> {
        let question = question
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Approve {title}?"));
        let mut fields = BTreeMap::from([
            ("owner".into(), Value::String(subject.to_owned())),
            ("reviewer".into(), Value::String(reviewer.into())),
            ("mode".into(), Value::String(mode.into())),
            ("question".into(), Value::String(question)),
            (
                "review_targets".into(),
                Value::Array(review_targets.iter().cloned().map(Value::String).collect()),
            ),
            (
                "decisions".into(),
                Value::Array(
                    if mode == "feedback" {
                        ["approved", "changes-requested"]
                    } else {
                        ["approved", "rejected"]
                    }
                    .into_iter()
                    .map(|value| Value::String(value.into()))
                    .collect(),
                ),
            ),
            (
                "mission_revision".into(),
                Value::String(run.revision.clone()),
            ),
            (
                "step_definition".into(),
                Value::String(definition_hash.to_owned()),
            ),
            ("attempt".into(), Value::from(attempt)),
        ]);
        let request_hash = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&fields)?));
        let operation = format!(
            "gate-operation/{}/{}",
            subject.replace('/', "."),
            &request_hash[..24]
        );
        fields.insert("operation".into(), Value::String(operation.clone()));
        let request = self.store.append_claim(&ClaimInput {
            subject: operation.clone(),
            kind: "gate.requested".into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "review-request:{}:{}",
                subject,
                &request_hash[..24]
            )),
        })?;
        let decision = self.store.latest_claim(&operation, Some("gate.result"))?;
        match decision.as_ref().and_then(|claim| {
            (claim.actor.as_deref() == Some(reviewer)
                && claim
                    .body
                    .pointer("/fields/request")
                    .and_then(Value::as_str)
                    == Some(request.id.as_str()))
            .then(|| {
                claim
                    .body
                    .pointer("/fields/verdict")
                    .and_then(Value::as_str)
            })
            .flatten()
        }) {
            Some("pass") => Ok(GateOutcome::Pass),
            Some("feedback") if mode == "feedback" => {
                let decision = decision.as_ref().expect("a feedback decision exists");
                let reason = decision
                    .body
                    .pointer("/fields/reason")
                    .and_then(Value::as_str)
                    .filter(|reason| !reason.trim().is_empty())
                    .context("a request for changes has no feedback text")?;
                self.apply_human_feedback(subject, attempt, reviewer, reason, &decision.id)?;
                Ok(GateOutcome::Pending)
            }
            Some("fail") => Ok(GateOutcome::Fail(human_review_failure_reason(
                decision.as_ref().expect("a failed decision exists"),
            ))),
            _ => Ok(GateOutcome::Pending),
        }
    }

    fn apply_human_feedback(
        &self,
        subject: &str,
        attempt: u32,
        reviewer: &str,
        reason: &str,
        decision_id: &str,
    ) -> Result<()> {
        let step = self
            .store
            .step_run(subject)?
            .context("the feedback step disappeared")?;
        if step.attempt != attempt {
            return Ok(());
        }
        let claimant = self
            .store
            .claims_for(subject, Some("work.claimed"))?
            .into_iter()
            .rev()
            .find(|claim| {
                claim
                    .body
                    .pointer("/fields/attempt")
                    .and_then(Value::as_u64)
                    == Some(u64::from(attempt))
            })
            .and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/claimant")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or(step.claimant)
            .or(step.carried_claimant)
            .or(step.assigned_to)
            .context("a feedback step has no claimant or assignee")?;
        let key = format!("human-feedback:{decision_id}:{claimant}");
        let message_id = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        self.store.append_claim(&ClaimInput {
            subject: format!("message/{}", &message_id[..16]),
            kind: "message.sent".into(),
            actor: Some(reviewer.into()),
            fields: BTreeMap::from([
                ("from".into(), Value::String(reviewer.into())),
                ("to".into(), Value::String(claimant)),
                (
                    "content".into(),
                    Value::String(format!(
                        "Reviewer requested changes to `{subject}`: {reason}. Claim the new attempt and address this feedback."
                    )),
                ),
                ("status".into(), Value::String("sent".into())),
                (
                    "title".into(),
                    Value::String("Human review requested changes".into()),
                ),
                ("in_reply_to".into(), Value::Null),
                (
                    "tags".into(),
                    Value::Array(vec![Value::String(format!("st3-feedback:{subject}"))]),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key),
        })?;
        if self
            .store
            .retry_step_for_feedback(subject, attempt, reason, reviewer)?
        {
            self.signal_changed();
        }
        Ok(())
    }

    fn step_timed_out(
        &self,
        view: &crate::model::StepRunView,
        step: &RuntimeStep<'_>,
    ) -> Result<bool> {
        let Some(timeout) = step.spec.timeout_ms else {
            return Ok(false);
        };
        let elapsed = view.execution_elapsed_ms;
        if elapsed >= timeout as u128 {
            return Ok(true);
        }
        if !matches!(view.status.as_str(), "claimed" | "working")
            || view.execution_started_at_unix_ms.is_none()
        {
            return Ok(false);
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let notify = self.notify.clone();
            let timeout_remaining = (timeout as u128).saturating_sub(elapsed);
            let lease_remaining = view
                .claim_expires_at_unix_ms
                .map_or(timeout_remaining, |expiry| expiry.saturating_sub(now_ms()));
            let remaining = timeout_remaining.min(lease_remaining).max(1) as u64;
            handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(remaining)).await;
                notify.notify_one();
            });
        }
        Ok(false)
    }

    fn reconcile_schedules(&self, desired: &[DesiredSubject]) -> Result<()> {
        for schedule in desired.iter().filter(|item| item.kind == "schedule") {
            self.isolate("schedule", &schedule.subject, || {
                self.reconcile_schedule(schedule)
            });
        }
        Ok(())
    }

    /// Record this schedule's next occurrence and arm its timer.
    fn reconcile_schedule(&self, schedule: &DesiredSubject) -> Result<()> {
        let Some(spec) = crate::graph::schedule_spec(&schedule.desired, &self.host) else {
            return Ok(());
        };
        if spec.stopped || spec.host != self.host {
            return Ok(());
        }
        if self.schedule_has_open_work(&schedule.subject)? {
            return Ok(());
        }
        let Some(revision) = self.store.selected_desired_revision(&schedule.subject)? else {
            return Ok(());
        };
        let reached = self
            .store
            .claims_for(&schedule.subject, Some("schedule.occurrence-reached"))?;
        let last = reached
            .iter()
            .filter(|claim| {
                claim
                    .body
                    .pointer("/fields/revision")
                    .and_then(Value::as_str)
                    == Some(&revision)
            })
            .filter_map(|claim| {
                claim
                    .body
                    .pointer("/fields/occurrence")
                    .and_then(Value::as_u64)
            })
            .max();
        let now = now_ms() as i64;
        let (occurrence, scheduled_at) = if let Some(at) = spec.at_unix_ms {
            if last.is_some() {
                return Ok(());
            }
            (0_u64, at)
        } else {
            let Some(interval) = spec.every_ms else {
                return Ok(());
            };
            let Some(anchor) = spec.anchor_unix_ms else {
                return Ok(());
            };
            let current = if now < anchor {
                0
            } else {
                ((now - anchor) as u64) / interval
            };
            let mut next = last.map_or(0, |value| value.saturating_add(1));
            if now >= anchor && next <= current {
                match spec.catch_up.as_str() {
                    "latest" => next = current,
                    "skip" => next = current.saturating_add(1),
                    "all" => {
                        let remaining = current.saturating_sub(next).saturating_add(1);
                        let max = spec.max_catch_up.unwrap_or(0);
                        if remaining > max as u64 {
                            self.record_once(
                                &schedule.subject,
                                "runtime.reconcile-decision",
                                BTreeMap::from([
                                    ("decision".into(), Value::String("raise".into())),
                                    ("reachability".into(), Value::String("unreachable".into())),
                                    (
                                        "reason".into(),
                                        Value::String("the schedule exceeds max-catch-up".into()),
                                    ),
                                ]),
                            )?;
                            // The schedule holds instead of starting a burst of missed work. That
                            // is a fault on the schedule, so it is seen rather than silently frozen.
                            anyhow::bail!(
                                "the missed occurrences exceed max-catch-up {max}; raise max-catch-up \
                                 or choose catch-up \"latest\" or \"skip\""
                            );
                        }
                    }
                    _ => return Ok(()),
                }
            }
            let offset = interval
                .checked_mul(next)
                .context("schedule occurrence overflow")?;
            let scheduled = anchor
                .checked_add(offset as i64)
                .context("schedule timestamp overflow")?;
            (next, scheduled)
        };
        let operation = format!("{}:{revision}:{occurrence}", schedule.subject);
        if !self
            .armed_schedules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(operation.clone())
        {
            return Ok(());
        }
        let request = self.store.append_claim(&ClaimInput {
            subject: schedule.subject.clone(),
            kind: "schedule.occurrence-scheduled".into(),
            actor: None,
            fields: BTreeMap::from([
                ("revision".into(), Value::String(revision.clone())),
                ("occurrence".into(), Value::from(occurrence)),
                (
                    "scheduled_at_unix_ms".into(),
                    Value::String(scheduled_at.to_string()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("clock-wake:{operation}")),
        })?;
        self.event_notify
            .send_modify(|generation| *generation = generation.saturating_add(1));
        let work = spec.work.clone();
        let store = self.store.clone();
        let notify = self.notify.clone();
        let event_notify = self.event_notify.clone();
        let armed = self.armed_schedules.clone();
        let schedule_subject = schedule.subject.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let delay = scheduled_at.saturating_sub(now_ms() as i64).max(0) as u64;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if store
                    .selected_desired_revision(&schedule_subject)
                    .ok()
                    .flatten()
                    .as_deref()
                    != Some(revision.as_str())
                {
                    let _ = store.append_claim(&ClaimInput {
                        subject: schedule_subject.clone(),
                        kind: "schedule.occurrence-cancelled".into(),
                        actor: None,
                        fields: BTreeMap::from([
                            ("revision".into(), Value::String(revision.clone())),
                            ("occurrence".into(), Value::from(occurrence)),
                            (
                                "reason".into(),
                                Value::String("the schedule revision changed".into()),
                            ),
                        ]),
                        evidence: vec![request.id.clone()],
                        expected_subject: None,
                        idempotency_key: Some(format!("clock-cancel:{operation}")),
                    });
                    armed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&operation);
                    signal_changed(&notify, &event_notify);
                    return;
                }
                let reached = store.append_claim(&ClaimInput {
                    subject: schedule_subject.clone(),
                    kind: "schedule.occurrence-reached".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("revision".into(), Value::String(revision.clone())),
                        ("occurrence".into(), Value::from(occurrence)),
                        (
                            "scheduled_at_unix_ms".into(),
                            Value::String(scheduled_at.to_string()),
                        ),
                    ]),
                    evidence: vec![request.id],
                    expected_subject: None,
                    idempotency_key: Some(format!("clock-reached:{operation}")),
                });
                if let (Ok(reached), Some(work)) = (reached, work) {
                    let _ = store.append_claim(&ClaimInput {
                        subject: schedule_subject.clone(),
                        kind: "schedule.work-requested".into(),
                        actor: None,
                        fields: BTreeMap::from([
                            ("revision".into(), Value::String(revision.clone())),
                            ("occurrence".into(), Value::from(occurrence)),
                            (
                                "mission".into(),
                                Value::String(format!("mission/{}", work.mission)),
                            ),
                            ("mission_revision".into(), Value::String(work.revision)),
                            ("workspace".into(), Value::String(work.workspace)),
                            (
                                "inputs".into(),
                                serde_json::to_value(work.inputs).unwrap_or_default(),
                            ),
                        ]),
                        evidence: vec![reached.id],
                        expected_subject: None,
                        idempotency_key: Some(format!("schedule-work-request:{operation}")),
                    });
                }
                armed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&operation);
                signal_changed(&notify, &event_notify);
            });
        } else {
            self.armed_schedules
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&operation);
        }
        Ok(())
    }

    fn schedule_has_open_work(&self, schedule: &str) -> Result<bool> {
        Ok(!self
            .store
            .pending_schedule_work_requests(schedule)?
            .is_empty()
            || self.store.schedule_has_active_started_run(schedule)?)
    }

    fn reconcile_scheduled_work(&self, desired: &[DesiredSubject]) -> Result<()> {
        for schedule in desired.iter().filter(|item| item.kind == "schedule") {
            self.isolate("schedule-work", &schedule.subject, || {
                self.reconcile_schedule_work(schedule)
            });
        }
        Ok(())
    }

    /// Start the work that this schedule's occurrences requested. A request that cannot start
    /// for a lasting reason is failed so the schedule can fire again. A request that waits for
    /// something this host has not received yet stays pending, and the schedule records why.
    fn reconcile_schedule_work(&self, schedule: &DesiredSubject) -> Result<()> {
        // Every peer replicates the same requests. Only the host that requested the work starts it.
        let requests = self
            .store
            .pending_schedule_work_requests(&schedule.subject)?
            .into_iter()
            .filter(|request| request.origin == self.host)
            .collect::<Vec<_>>();
        // A stopped schedule's queued work is cancelled, so declaring the schedule again does not
        // start work that was requested before it stopped.
        if intake_is_stopped(schedule, &self.host) {
            for request in requests {
                self.fail_schedule_work(
                    schedule,
                    &request.id,
                    "schedule-stopped",
                    "the schedule stopped before this work started",
                )?;
            }
            return Ok(());
        }
        if requests.is_empty()
            || self
                .store
                .schedule_has_active_started_run(&schedule.subject)?
        {
            return Ok(());
        }
        let mut waiting = Vec::new();
        for request in requests {
            if self
                .store
                .schedule_has_active_started_run(&schedule.subject)?
            {
                break;
            }
            let fields = request.body.get("fields").unwrap_or(&request.body);
            let field = |name: &str| fields.get(name).and_then(Value::as_str);
            let (Some(mission), Some(revision), Some(root)) = (
                field("mission"),
                field("mission_revision"),
                field("workspace"),
            ) else {
                self.fail_schedule_work(
                    schedule,
                    &request.id,
                    "invalid-request",
                    "the request names no mission, revision, or workspace",
                )?;
                continue;
            };
            let suffix = &hex::encode(sha2::Sha256::digest(request.id.as_bytes()))[..16];
            let workspace = Path::new(root).join(suffix).to_string_lossy().into_owned();
            let inputs = serde_json::from_value(fields.get("inputs").cloned().unwrap_or_default())
                .unwrap_or_default();
            let request_value = MissionRunRequest {
                mission: mission.into(),
                revision: Some(revision.into()),
                workspace,
                requester: Some(format!("daemon/{}", self.host)),
                mode: None,
                inputs,
                idempotency_key: format!("schedule-work:{}", request.id),
            };
            let created = match &schedule.owner_run {
                Some(owner) => match self.store.mission_run(owner)? {
                    Some(parent) => self.store.create_child_mission_run(
                        &request_value,
                        &parent,
                        &schedule.subject,
                        None,
                    ),
                    None => {
                        waiting.push(format!(
                            "request {}: its owner run {owner} is not stored here yet",
                            request.id
                        ));
                        continue;
                    }
                },
                None => self.store.create_mission_run(&request_value),
            };
            let run = match created {
                Ok(run) => run,
                Err(error) if error.code == "mission-run-capacity" => continue,
                Err(error) if start_waits_for_replication(&error) => {
                    waiting.push(format!("request {}: {error}", request.id));
                    continue;
                }
                Err(error) => {
                    self.fail_schedule_work(schedule, &request.id, error.code, &error.to_string())?;
                    continue;
                }
            };
            self.store.append_claim(&ClaimInput {
                subject: schedule.subject.clone(),
                kind: "schedule.work-started".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("request".into(), Value::String(request.id.clone())),
                    ("mission_run".into(), Value::String(run.subject)),
                ]),
                evidence: vec![request.id],
                expected_subject: None,
                idempotency_key: Some(format!("schedule-work-started:{}", run.id)),
            })?;
        }
        anyhow::ensure!(waiting.is_empty(), "{}", waiting.join("; "));
        Ok(())
    }

    fn fail_schedule_work(
        &self,
        schedule: &DesiredSubject,
        request: &str,
        code: &str,
        reason: &str,
    ) -> Result<()> {
        self.store.append_claim(&ClaimInput {
            subject: schedule.subject.clone(),
            kind: "schedule.work-failed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("request".into(), Value::String(request.into())),
                ("code".into(), Value::String(code.into())),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: vec![request.into()],
            expected_subject: None,
            idempotency_key: Some(format!("schedule-work-failed:{request}")),
        })?;
        Ok(())
    }

    fn reconcile_subscription_missions(&self, desired: &[DesiredSubject]) -> Result<()> {
        for item in desired.iter().filter(|item| item.kind == "subscription") {
            self.isolate("subscription", &item.subject, || {
                self.reconcile_subscription_mission(item)
            });
        }
        Ok(())
    }

    /// Start the missions that this subscription's deliveries requested.
    fn reconcile_subscription_mission(&self, item: &DesiredSubject) -> Result<()> {
        let Some(spec) = crate::graph::subscription_spec(&item.desired) else {
            return Ok(());
        };
        // Every peer replicates the same requests. Only the declaring host starts their runs.
        if self
            .store
            .selected_desired_origin(&item.subject)?
            .as_deref()
            != Some(self.host.as_str())
        {
            return Ok(());
        }
        if spec.stopped {
            self.cancel_unstarted_subscription_requests(&item.subject)?;
            return Ok(());
        }
        if spec.delivery != "mission" {
            return Ok(());
        }
        let requests = self
            .store
            .pending_subscription_mission_requests(&item.subject)?;
        if requests.is_empty() {
            return Ok(());
        }
        let is_held = |request: &crate::model::ClaimRecord| {
            request
                .body
                .pointer("/fields/held")
                .and_then(Value::as_bool)
                == Some(true)
        };
        let released = if requests.iter().any(is_held) {
            self.store
                .claims_for(&item.subject, Some("subscription.mission-request-released"))?
                .into_iter()
                .filter_map(|claim| {
                    claim
                        .body
                        .pointer("/fields/request")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let mut held = BTreeMap::<String, usize>::new();
        let mut waiting = Vec::new();
        for request in requests {
            if is_held(&request) && !released.contains(&request.id) {
                let observation = request
                    .body
                    .pointer("/evidence/0")
                    .and_then(Value::as_str)
                    .unwrap_or(request.id.as_str())
                    .to_owned();
                *held.entry(observation).or_default() += 1;
                continue;
            }
            let deferral = self
                .store
                .subscription_mission_deferral(&item.subject, &request.id)?;
            if deferral.is_some_and(|(deadline, _)| deadline > now_ms()) {
                continue;
            }
            let fields = request.body.get("fields").unwrap_or(&request.body);
            let field = |name: &str| fields.get(name).and_then(Value::as_str);
            let (
                Some(mission),
                Some(revision),
                Some(resource),
                Some(discovery),
                Some(input),
                Some(root),
            ) = (
                field("mission"),
                field("mission_revision"),
                field("resource"),
                field("discovery"),
                field("resource_input"),
                field("workspace"),
            )
            else {
                self.fail_subscription_request(
                    item,
                    &request.id,
                    "invalid-request",
                    "the request lacks a mission, revision, resource, discovery, input, or workspace",
                )?;
                continue;
            };
            let requester = field("requester")
                .map(str::to_owned)
                .unwrap_or_else(|| format!("daemon/{}", self.host));
            let suffix = &hex::encode(sha2::Sha256::digest(request.id.as_bytes()))[..16];
            let workspace = Path::new(root).join(suffix).to_string_lossy().into_owned();
            let request_value = MissionRunRequest {
                mission: mission.into(),
                revision: Some(revision.into()),
                workspace,
                requester: Some(requester),
                mode: None,
                inputs: BTreeMap::from([(input.into(), format!("{resource}@{discovery}"))]),
                idempotency_key: format!("subscription-mission:{}", request.id),
            };
            let created = match &item.owner_run {
                Some(owner) => match self.store.mission_run(owner)? {
                    Some(parent) => self.store.create_child_mission_run(
                        &request_value,
                        &parent,
                        &item.subject,
                        None,
                    ),
                    None => {
                        waiting.push(format!(
                            "request {}: its owner run {owner} is not stored here yet",
                            request.id
                        ));
                        continue;
                    }
                },
                None => self.store.create_mission_run(&request_value),
            };
            let run = match created {
                Ok(run) => run,
                Err(error) if error.code == "mission-run-capacity" => {
                    let attempt =
                        deferral.map_or(1_u32, |(_, attempts)| attempts.saturating_add(1));
                    let delay_ms =
                        1_000_u128.saturating_mul(1_u128 << attempt.saturating_sub(1).min(6));
                    let not_before = now_ms().saturating_add(delay_ms);
                    self.store.append_claim(&ClaimInput {
                        subject: item.subject.clone(),
                        kind: "subscription.mission-deferred".into(),
                        actor: None,
                        fields: BTreeMap::from([
                            ("request".into(), Value::String(request.id.clone())),
                            ("not_before_unix_ms".into(), Value::from(not_before as u64)),
                        ]),
                        evidence: vec![request.id.clone()],
                        expected_subject: None,
                        idempotency_key: Some(format!(
                            "subscription-mission-deferred:{}:{attempt}",
                            request.id
                        )),
                    })?;
                    continue;
                }
                // The request stays pending and starts once replication delivers what it names.
                Err(error) if start_waits_for_replication(&error) => {
                    waiting.push(format!("request {}: {error}", request.id));
                    continue;
                }
                Err(error) => {
                    self.fail_subscription_request(
                        item,
                        &request.id,
                        error.code,
                        &error.to_string(),
                    )?;
                    continue;
                }
            };
            self.store.append_claim(&ClaimInput {
                subject: item.subject.clone(),
                kind: "subscription.mission-started".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("request".into(), Value::String(request.id.clone())),
                    ("mission_run".into(), Value::String(run.subject)),
                ]),
                evidence: vec![request.id],
                expected_subject: None,
                idempotency_key: Some(format!("subscription-mission-started:{}", run.id)),
            })?;
        }
        for (observation, count) in held {
            self.request_held_subscription_attention(&item.subject, &observation, count)?;
        }
        anyhow::ensure!(waiting.is_empty(), "{}", waiting.join("; "));
        Ok(())
    }

    fn fail_subscription_request(
        &self,
        item: &DesiredSubject,
        request: &str,
        code: &str,
        reason: &str,
    ) -> Result<()> {
        self.store.append_claim(&ClaimInput {
            subject: item.subject.clone(),
            kind: "subscription.mission-failed".into(),
            actor: None,
            fields: BTreeMap::from([
                ("request".into(), Value::String(request.into())),
                ("code".into(), Value::String(code.into())),
                ("reason".into(), Value::String(reason.into())),
            ]),
            evidence: vec![request.into()],
            expected_subject: None,
            idempotency_key: Some(format!("subscription-mission-failed:{request}")),
        })?;
        Ok(())
    }

    /// Close each request that a stopped subscription recorded but never started, so a later
    /// declaration of the same subscription cannot start it.
    fn cancel_unstarted_subscription_requests(&self, subscription: &str) -> Result<()> {
        for request in self
            .store
            .pending_subscription_mission_requests(subscription)?
        {
            self.store.append_claim(&ClaimInput {
                subject: subscription.into(),
                kind: "subscription.mission-request-cancelled".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("request".into(), Value::String(request.id.clone())),
                    (
                        "reason".into(),
                        Value::String("the subscription stopped".into()),
                    ),
                ]),
                evidence: vec![request.id.clone()],
                expected_subject: None,
                idempotency_key: Some(format!("subscription-request-cancelled:{}", request.id)),
            })?;
        }
        Ok(())
    }

    /// Ask a person once per observation to release or cancel the requests it held.
    fn request_held_subscription_attention(
        &self,
        subscription: &str,
        observation: &str,
        count: usize,
    ) -> Result<()> {
        let digest = hex::encode(sha2::Sha256::digest(
            format!("held-subscription-requests:{subscription}:{observation}").as_bytes(),
        ));
        let attention_subject = format!("attention/{}", &digest[..32]);
        if self.store.attention_request(&attention_subject)?.is_some() {
            return Ok(());
        }
        self.store.request_attention(
            &attention_subject,
            &AttentionRequest {
                reviewer: "person/operator".into(),
                title: "A subscription is holding mission requests".into(),
                reason: format!(
                    "One observation for {subscription} requested more than {} mission runs, so {count} wait for a person. List them with `st missions requests {subscription}`, then release or cancel each one.",
                    crate::store::MAX_OBSERVATION_DELIVERIES
                ),
                severity: "warning".into(),
                targets: vec![subscription.into()],
                actor: "agent/st3/reconciler".into(),
                idempotency_key: format!("held-subscription-requests:{}", &digest[..32]),
            },
        )?;
        self.signal_changed();
        Ok(())
    }

    fn reconcile_resource_observers(&self, desired: &[DesiredSubject]) -> Result<()> {
        let observer_resources = desired
            .iter()
            .filter(|item| item.kind == "observer")
            .filter_map(|item| {
                crate::graph::observer_spec(&item.desired)
                    .filter(|spec| !spec.stopped)
                    .map(|spec| (item.subject.clone(), spec.resource))
            })
            .collect::<HashMap<_, _>>();
        let mut subscriptions_by_resource =
            HashMap::<String, Vec<(String, SubscriptionSpec)>>::new();
        for (subject, subscription) in desired
            .iter()
            .filter(|item| item.kind == "subscription")
            .filter_map(|item| {
                crate::graph::subscription_spec(&item.desired)
                    .filter(|spec| !spec.stopped)
                    .map(|spec| (item.subject.clone(), spec))
            })
        {
            if let Some(resource) = observer_resources.get(&subscription.observer) {
                subscriptions_by_resource
                    .entry(resource.clone())
                    .or_default()
                    .push((subject, subscription));
            }
        }
        for observer in desired.iter().filter(|item| item.kind == "observer") {
            self.isolate("observer", &observer.subject, || {
                self.reconcile_resource_observer(observer, &subscriptions_by_resource)
            });
        }
        Ok(())
    }

    /// Settle a stopped observer, or arm this observer's next observation.
    fn reconcile_resource_observer(
        &self,
        observer: &DesiredSubject,
        subscriptions_by_resource: &HashMap<String, Vec<(String, SubscriptionSpec)>>,
    ) -> Result<()> {
        let Some(mut spec) = crate::graph::observer_spec(&observer.desired) else {
            return Ok(());
        };
        let selected = subscriptions_by_resource
            .get(&spec.resource)
            .cloned()
            .unwrap_or_default();
        if spec.stopped {
            let is_stopped = self
                .store
                .latest_actual_value(&observer.subject)?
                .and_then(|actual| {
                    actual
                        .get("state")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("stopped");
            if !is_stopped {
                self.store.append_claim(&ClaimInput {
                    subject: observer.subject.clone(),
                    kind: "observer.state".into(),
                    actor: None,
                    fields: BTreeMap::from([("state".into(), Value::String("stopped".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })?;
            }
            return Ok(());
        }
        if self
            .store
            .selected_desired_origin(&observer.subject)?
            .as_deref()
            != Some(self.host.as_str())
        {
            return Ok(());
        }
        spec.fields.extend(
            selected
                .iter()
                .filter(|(_, subscription)| subscription.observer == observer.subject)
                .flat_map(|(_, subscription)| subscription.fields.iter().cloned()),
        );
        spec.fields.sort();
        spec.fields.dedup();
        let Some(revision) = self.store.selected_desired_revision(&observer.subject)? else {
            return Ok(());
        };
        let observer_actual = self.store.latest_actual_value(&observer.subject)?;
        let refresh_attempt = self
            .store
            .pending_observer_refresh_attempt(&observer.subject)?;
        // A permanent error is not polled again on the same revision. A refresh request still
        // polls it once, so the observer and its subscriptions can recover without a new revision.
        if refresh_attempt.is_none()
            && observer_actual.as_ref().is_some_and(|actual| {
                actual.get("state").and_then(Value::as_str) == Some("degraded")
                    && actual.get("revision").and_then(Value::as_str) == Some(revision.as_str())
                    && actual
                        .get("error_code")
                        .and_then(Value::as_str)
                        .is_some_and(permanent_observation_error)
            })
        {
            return Ok(());
        }
        let deadline_key = format!("{}:{revision}", observer.subject);
        let next_check = refresh_attempt
            .as_ref()
            .map(|_| now_ms())
            .unwrap_or_else(|| {
                self.observer_deadlines
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&deadline_key)
                    .copied()
                    .or_else(|| {
                        observer_actual
                            .as_ref()
                            .and_then(|actual| actual.get("next_check_unix_ms"))
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse().ok())
                    })
                    .unwrap_or_else(now_ms)
            });
        let operation = format!(
            "{}:{revision}:{}",
            observer.subject,
            refresh_attempt.as_deref().unwrap_or("scheduled")
        );
        if !self
            .armed_observers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(operation.clone())
        {
            return Ok(());
        }
        let store = self.store.clone();
        let provider = self.resource_provider.clone();
        let notify = self.notify.clone();
        let event_notify = self.event_notify.clone();
        let armed = self.armed_observers.clone();
        let deadlines = self.observer_deadlines.clone();
        let cursors = self.observer_cursors.clone();
        let observer_subject = observer.subject.clone();
        let observer_owner_run = observer.owner_run.clone();
        let previous_facts = self
            .store
            .latest_actual_value(&spec.resource)?
            .and_then(|actual| actual.get("facts").cloned());
        let cursor = self
            .observer_cursors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&deadline_key)
            .cloned()
            .unwrap_or_else(|| {
                observer_actual
                    .as_ref()
                    .and_then(|actual| actual.get("cursor"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let delay = next_check.saturating_sub(now_ms()).min(u64::MAX as u128) as u64;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if store
                    .selected_desired_revision(&observer_subject)
                    .ok()
                    .flatten()
                    .as_deref()
                    != Some(revision.as_str())
                {
                    armed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&operation);
                    signal_changed(&notify, &event_notify);
                    return;
                }
                let request = ObservationRequest {
                    provider: spec.provider.clone(),
                    locator: spec.locator.clone(),
                    fields: spec.fields.iter().cloned().collect(),
                    cursor,
                    previous_facts,
                    every_ms: spec.every_ms,
                };
                match provider.observe(request).await {
                    Ok(mut observation) => {
                        if let Some(every_ms) = spec.every_ms {
                            observation.next_check_unix_ms =
                                now_ms().saturating_add(every_ms as u128);
                        }
                        match store.record_resource_observation(
                            &observer_subject,
                            &revision,
                            refresh_attempt.as_deref(),
                            &spec.resource,
                            observation.cursor.as_deref(),
                            &observation.facts,
                            observation.next_check_unix_ms,
                            &selected,
                        ) {
                            Ok(_) => {
                                deadlines
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .insert(deadline_key.clone(), observation.next_check_unix_ms);
                                cursors
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .insert(deadline_key.clone(), observation.cursor);
                            }
                            Err(error) => {
                                let retry_at = now_ms().saturating_add(60_000);
                                deadlines
                                    .lock()
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .insert(deadline_key.clone(), retry_at);
                                let reason = error.to_string();
                                let failure_hash = hex::encode(sha2::Sha256::digest(
                                    format!("{revision}:{}:{reason}", error.code).as_bytes(),
                                ));
                                let mut fields = BTreeMap::from([
                                    ("state".into(), Value::String("degraded".into())),
                                    ("reason".into(), Value::String(reason)),
                                    ("error_code".into(), Value::String(error.code.into())),
                                    ("revision".into(), Value::String(revision.clone())),
                                ]);
                                if let Some(attempt) = &refresh_attempt {
                                    fields.insert("attempt".into(), Value::String(attempt.clone()));
                                }
                                // A refresh that meets the same failure still records its
                                // attempt, which serves the refresh request.
                                let key = match &refresh_attempt {
                                    Some(attempt) => format!(
                                        "observer-rejected:{}:{attempt}",
                                        &failure_hash[..20]
                                    ),
                                    None => format!("observer-rejected:{}", &failure_hash[..20]),
                                };
                                let _ = store.append_claim(&ClaimInput {
                                    subject: observer_subject.clone(),
                                    kind: "observer.state".into(),
                                    actor: None,
                                    fields,
                                    evidence: Vec::new(),
                                    expected_subject: None,
                                    idempotency_key: Some(key),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        let rate_limit = error.downcast_ref::<ProviderRateLimit>();
                        let retry_at = rate_limit.map_or_else(
                            || now_ms().saturating_add(60_000),
                            |limit| limit.retry_at_unix_ms.max(now_ms().saturating_add(1_000)),
                        );
                        let reason = error.to_string();
                        deadlines
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(deadline_key.clone(), retry_at);
                        if rate_limit.is_some_and(|limit| limit.unauthenticated)
                            || error.downcast_ref::<ProviderUnauthenticated>().is_some()
                            || observer_unreachable_since(&store, &observer_subject)
                                .ok()
                                .flatten()
                                .is_some_and(|since| now_ms().saturating_sub(since) >= 3_600_000)
                        {
                            let _ = request_observer_attention(
                                &store,
                                &observer_subject,
                                observer_owner_run.as_deref(),
                                &revision,
                                &reason,
                            );
                        }
                        let unchanged_failure = store
                            .latest_actual_value(&observer_subject)
                            .ok()
                            .flatten()
                            .is_some_and(|actual| {
                                actual.get("state").and_then(Value::as_str) == Some("unreachable")
                                    && actual.get("reason").and_then(Value::as_str)
                                        == Some(reason.as_str())
                            });
                        if unchanged_failure && refresh_attempt.is_none() {
                            armed
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .remove(&operation);
                            signal_changed(&notify, &event_notify);
                            return;
                        }
                        let failure_hash = hex::encode(sha2::Sha256::digest(
                            format!("{operation}:{reason}").as_bytes(),
                        ));
                        let mut fields = BTreeMap::from([
                            ("state".into(), Value::String("unreachable".into())),
                            ("reason".into(), Value::String(reason)),
                            ("revision".into(), Value::String(revision.clone())),
                            (
                                "next_check_unix_ms".into(),
                                Value::String(retry_at.to_string()),
                            ),
                        ]);
                        if let Some(attempt) = &refresh_attempt {
                            fields.insert("attempt".into(), Value::String(attempt.clone()));
                        }
                        let _ = store.append_claim(&ClaimInput {
                            subject: observer_subject.clone(),
                            kind: "observer.state".into(),
                            actor: None,
                            fields,
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!(
                                "observer-failure:{}",
                                &failure_hash[..20]
                            )),
                        });
                    }
                }
                armed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&operation);
                signal_changed(&notify, &event_notify);
            });
        } else {
            self.armed_observers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&operation);
        }
        Ok(())
    }

    fn evaluate_gate(&self, stage: &GateContext, gate: &GateSpec) -> Result<GateOutcome> {
        let outcome = match gate {
            GateSpec::Exists { subject, .. } => {
                self.ensure_file_observation(subject)?;
                if self.subject_value(subject)?.is_some_and(|actual| {
                    actual_field(&actual, "status").and_then(Value::as_str) != Some("unreadable")
                }) {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Pending
                }
            }
            GateSpec::Empty { subject, .. } => {
                let members = self
                    .store
                    .desired_subjects()?
                    .into_iter()
                    .filter(|item| {
                        item.owner_run.as_deref() == Some(subject) && item.member.is_some()
                    })
                    .collect::<Vec<_>>();
                let mut empty = true;
                for member in members {
                    if self
                        .store
                        .latest_actual_value(&member.subject)?
                        .is_some_and(|value| {
                            !matches!(
                                actual_field(&value, "status").and_then(Value::as_str),
                                Some("absent" | "stopped" | "exited")
                            )
                        })
                    {
                        empty = false;
                    }
                }
                if empty {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Pending
                }
            }
            GateSpec::Field {
                path,
                subject,
                operator,
                value,
                ..
            } => {
                self.ensure_file_observation(subject)?;
                let Some(actual) = self.subject_value(subject)? else {
                    return Ok(GateOutcome::Pending);
                };
                let found = observed_field_value(&actual, subject, path);
                let Some(found) = found.as_ref() else {
                    return Ok(GateOutcome::Pending);
                };
                if compare_value(found, operator, value) {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Pending
                }
            }
            GateSpec::Every {
                path,
                subject,
                fields,
                ..
            }
            | GateSpec::NotEvery {
                path,
                subject,
                fields,
                ..
            } => {
                self.ensure_file_observation(subject)?;
                let Some(actual) = self.subject_value(subject)? else {
                    return Ok(GateOutcome::Pending);
                };
                let Some(items) = observed_field_value(&actual, subject, path)
                    .and_then(|value| value.as_array().cloned())
                else {
                    return Ok(GateOutcome::Pending);
                };
                let every = items.iter().all(|item| {
                    fields.iter().all(|field| {
                        actual_field(item, &field.path).is_some_and(|found| {
                            compare_value(found, &field.operator, &field.value)
                        })
                    })
                });
                let pass = matches!(gate, GateSpec::Every { .. }) == every;
                if pass {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Pending
                }
            }
            GateSpec::Has { subject, text, .. } | GateSpec::Lacks { subject, text, .. } => {
                self.ensure_file_observation(subject)?;
                let Some(content) = self.subject_text(subject)? else {
                    return Ok(GateOutcome::Pending);
                };
                let contains = content.contains(text);
                let pass = matches!(gate, GateSpec::Has { .. }) == contains;
                if pass {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Pending
                }
            }
            GateSpec::Deadline { duration_ms, .. } => {
                let elapsed = now_ms().saturating_sub(stage.started_at_unix_ms);
                if elapsed >= *duration_ms as u128 {
                    GateOutcome::Fail(format!("deadline expired after {duration_ms}ms"))
                } else {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let notify = self.notify.clone();
                        let remaining = (*duration_ms as u128).saturating_sub(elapsed) as u64;
                        handle.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(remaining)).await;
                            notify.notify_one();
                        });
                    }
                    GateOutcome::Pass
                }
            }
            GateSpec::Mechanical {
                name,
                command,
                host,
                workspace,
                environment,
                time_limit_ms,
                ..
            } => self.run_mechanical(
                stage,
                name,
                command,
                host,
                workspace,
                environment,
                *time_limit_ms,
            )?,
            GateSpec::Llm {
                name,
                model,
                host,
                workspace,
                tools,
                environment,
                token_budget,
                time_limit_ms,
                prompt,
            } => self.run_llm_gate(
                stage,
                name,
                model,
                host,
                workspace,
                tools,
                environment,
                *token_budget,
                *time_limit_ms,
                prompt,
            )?,
            GateSpec::Human { reviewer, .. } => {
                let operation = gate_operation_subject(
                    stage,
                    crate::graph::gate_name(gate),
                    &serde_json::to_value(gate)?,
                )?;
                let decision = self.store.latest_claim(&operation, Some("gate.result"))?;
                match decision.as_ref().and_then(|claim| {
                    (claim.actor.as_deref() == Some(reviewer.as_str()))
                        .then(|| {
                            claim
                                .body
                                .pointer("/fields/verdict")
                                .and_then(Value::as_str)
                        })
                        .flatten()
                }) {
                    Some("pass") => GateOutcome::Pass,
                    Some("fail") => GateOutcome::Fail(human_review_failure_reason(
                        decision.as_ref().expect("a failed decision exists"),
                    )),
                    _ => GateOutcome::Pending,
                }
            }
        };
        if matches!(outcome, GateOutcome::Pass | GateOutcome::Fail(_)) {
            let name = crate::graph::gate_name(gate);
            let digest = hex::encode(sha2::Sha256::digest(
                format!("{}:{name}", stage.subject).as_bytes(),
            ));
            let (verdict, reason) = match &outcome {
                GateOutcome::Pass => ("pass", None),
                GateOutcome::Fail(reason) => ("fail", Some(reason.clone())),
                GateOutcome::Pending => unreachable!(),
            };
            let mut fields = BTreeMap::from([
                ("stage".into(), Value::String(stage.subject.clone())),
                ("gate".into(), Value::String(name.to_owned())),
                ("verdict".into(), Value::String(verdict.into())),
            ]);
            if let Some(reason) = reason {
                fields.insert("reason".into(), Value::String(reason));
            }
            let subject = format!("gate-operation/predicate/{}", &digest[..32]);
            fields.insert("operation".into(), Value::String(subject.clone()));
            self.record_once(&subject, "gate.result", fields)?;
        }
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_mechanical(
        &self,
        stage: &GateContext,
        name: &str,
        command: &str,
        host: &str,
        workspace: &str,
        environment: &BTreeMap<String, String>,
        time_limit_ms: u64,
    ) -> Result<GateOutcome> {
        let result_subject = gate_result_subject(
            stage,
            name,
            &serde_json::json!({
                "type": "mechanical",
                "command": command,
                "host": host,
                "workspace": workspace,
                "environment": environment,
                "time_limit_ms": time_limit_ms,
            }),
        )?;
        let runtime_id = result_subject.replace('/', ".");
        if let Some(result) = self
            .store
            .latest_claim(&result_subject, Some("gate.result"))?
        {
            let verdict = result
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str)
                .unwrap_or("fail");
            let reason = result
                .body
                .pointer("/fields/reason")
                .and_then(Value::as_str)
                .unwrap_or("the mechanical gate failed");
            return Ok(if verdict == "pass" {
                GateOutcome::Pass
            } else {
                GateOutcome::Fail(reason.into())
            });
        }
        if host != self.host {
            return Ok(GateOutcome::Pending);
        }
        if let Some(requested) = self
            .store
            .latest_claim(&result_subject, Some("gate.requested"))?
        {
            let elapsed = now_ms().saturating_sub(requested.accepted_at_unix_ms);
            if elapsed >= time_limit_ms as u128 {
                self.stop_gate_runner(&result_subject, true)?;
                let reason = format!("mechanical gate `{name}` exceeded {time_limit_ms}ms");
                self.record_once(
                    &result_subject,
                    "gate.result",
                    BTreeMap::from([
                        ("verdict".into(), Value::String("fail".into())),
                        ("reason".into(), Value::String(reason.clone())),
                    ]),
                )?;
                return Ok(GateOutcome::Fail(reason));
            }
            match self.runtime.observe_exec(&runtime_id)? {
                Some(observation) if observation.status == "running" => {
                    self.arm_gate_poll();
                    return Ok(GateOutcome::Pending);
                }
                Some(observation) if observation.status == "exited" => {
                    let exit_code = self.gate_exit_code(&result_subject, &observation)?;
                    let verdict = if exit_code == Some(0) { "pass" } else { "fail" };
                    let reason = match exit_code {
                        Some(_) => format!("mechanical gate `{name}` {verdict}"),
                        None => format!(
                            "mechanical gate `{name}` {verdict}: it exited without an exit status"
                        ),
                    };
                    self.record_once(
                        &result_subject,
                        "gate.result",
                        BTreeMap::from([
                            ("verdict".into(), Value::String(verdict.into())),
                            ("reason".into(), Value::String(reason.clone())),
                        ]),
                    )?;
                    return Ok(if verdict == "pass" {
                        GateOutcome::Pass
                    } else {
                        GateOutcome::Fail(reason)
                    });
                }
                _ => {
                    self.arm_gate_poll();
                    return Ok(GateOutcome::Pending);
                }
            }
        }

        self.record_once(
            &result_subject,
            "gate.requested",
            BTreeMap::from([
                ("status".into(), Value::String("requested".into())),
                ("runner".into(), Value::String("exec".into())),
            ]),
        )?;
        let member = MemberSpec {
            kind: MemberKind::Exec,
            host: host.into(),
            runtime_id,
            workspace: workspace.into(),
            workspace_create: false,
            cwd: workspace.into(),
            terminal: false,
            launch: LaunchSpec::Shell(command.into()),
            environment: environment.clone(),
            tags: BTreeMap::new(),
            display_name: None,
            lifecycle: MemberLifecycle::Service,
            restart: RestartType::Never,
            restart_intensity: RestartIntensity::default(),
            shutdown_timeout_ms: 5_000,
            driver: Some("mechanical-gate".into()),
        };
        let desired = DesiredSubject {
            subject: result_subject,
            kind: "gate".into(),
            desired: Value::Null,
            member: Some(member.clone()),
            owner_run: None,
            owner_generation: None,
            owner_step: None,
        };
        self.perform_start(&desired, &member, "the mechanical gate was requested")?;
        self.arm_gate_poll();
        Ok(GateOutcome::Pending)
    }

    /// An exited gate runner's exit code. A runner that outlived a daemon restart is no longer
    /// this daemon's child, so the exec runtime finds it gone without a status. Its
    /// `st3 driver exec` wrapper reports the status to the graph before it exits, so that report
    /// decides. With neither, the wrapper itself was killed, and the runner has no exit code.
    fn gate_exit_code(
        &self,
        subject: &str,
        observation: &RuntimeObservation,
    ) -> Result<Option<i64>> {
        if observation.exit_code.is_some() {
            return Ok(observation.exit_code);
        }
        Ok(self
            .store
            .claims_for(subject, Some("runtime.observed"))?
            .into_iter()
            .rev()
            .find(|claim| claim.actor.as_deref() == Some(subject))
            .and_then(|claim| {
                claim
                    .body
                    .pointer("/fields/exit_code")
                    .and_then(Value::as_i64)
            }))
    }

    fn arm_gate_poll(&self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if self.gate_poll_armed.swap(true, Ordering::AcqRel) {
                return;
            }
            let notify = self.notify.clone();
            let armed = self.gate_poll_armed.clone();
            handle.spawn(async move {
                tokio::time::sleep(GATE_POLL_INTERVAL).await;
                armed.store(false, Ordering::Release);
                notify.notify_one();
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_llm_gate(
        &self,
        stage: &GateContext,
        name: &str,
        model: &str,
        host: &str,
        workspace: &str,
        tools: &[String],
        environment: &BTreeMap<String, String>,
        token_budget: u64,
        time_limit_ms: u64,
        prompt: &str,
    ) -> Result<GateOutcome> {
        let result_subject = gate_result_subject(
            stage,
            name,
            &serde_json::json!({
                "type": "llm",
                "model": model,
                "host": host,
                "workspace": workspace,
                "tools": tools,
                "environment": environment,
                "token_budget": token_budget,
                "time_limit_ms": time_limit_ms,
                "prompt": prompt,
            }),
        )?;
        let runtime_id = result_subject.replace('/', ".");
        if let Some(result) = self.store.latest_actual_value(&result_subject)? {
            let verdict = actual_field(&result, "verdict")
                .and_then(Value::as_str)
                .unwrap_or("fail");
            let reason = actual_field(&result, "reason")
                .and_then(Value::as_str)
                .unwrap_or("the LLM gate failed");
            if let Some(token_usage) = actual_field(&result, "token_usage").and_then(Value::as_u64)
            {
                self.stop_gate_runner(&result_subject, false)?;
                if token_usage > token_budget {
                    return Ok(GateOutcome::Fail(format!(
                        "LLM gate `{name}` used {token_usage} tokens, above its {token_budget} token budget"
                    )));
                }
                return Ok(if verdict == "pass" {
                    GateOutcome::Pass
                } else {
                    GateOutcome::Fail(reason.into())
                });
            }
            // A runner that posted its verdict but never exits still has the gate's time limit.
            let timed_out = self
                .store
                .latest_claim(&result_subject, Some("gate.requested"))?
                .is_some_and(|requested| {
                    now_ms().saturating_sub(requested.accepted_at_unix_ms)
                        >= u128::from(time_limit_ms)
                });
            match self.runtime.observe_exec(&runtime_id)? {
                Some(observation) if observation.status == "running" && !timed_out => {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let notify = self.notify.clone();
                        handle.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            notify.notify_one();
                        });
                    }
                    return Ok(GateOutcome::Pending);
                }
                Some(observation) if observation.status == "indeterminate" && !timed_out => {
                    return Ok(GateOutcome::Pending);
                }
                _ => {}
            }
            if timed_out {
                self.stop_gate_runner(&result_subject, true)?;
            }
            let token_usage = self
                .runtime
                .read_exec_log(&runtime_id)?
                .as_deref()
                .and_then(structured_token_usage);
            let (verdict, reason, token_usage) = match token_usage {
                Some(token_usage) if token_usage > token_budget => (
                    "fail",
                    format!(
                        "LLM gate `{name}` used {token_usage} tokens, above its {token_budget} token budget"
                    ),
                    token_usage,
                ),
                Some(token_usage) => (verdict, reason.into(), token_usage),
                None if timed_out => (
                    "fail",
                    format!("LLM gate `{name}` exceeded {time_limit_ms}ms"),
                    0,
                ),
                None => (
                    "fail",
                    format!("LLM gate `{name}` did not report structured token usage"),
                    0,
                ),
            };
            self.record_once(
                &result_subject,
                "gate.result",
                BTreeMap::from([
                    ("verdict".into(), Value::String(verdict.into())),
                    ("reason".into(), Value::String(reason.clone())),
                    ("token_usage".into(), Value::from(token_usage)),
                ]),
            )?;
            return Ok(if verdict == "pass" {
                GateOutcome::Pass
            } else {
                GateOutcome::Fail(reason)
            });
        }
        if host != self.host {
            return Ok(GateOutcome::Pending);
        }
        if let Some(requested) = self
            .store
            .latest_claim(&result_subject, Some("gate.requested"))?
        {
            let elapsed = now_ms().saturating_sub(requested.accepted_at_unix_ms);
            if elapsed >= time_limit_ms as u128 {
                self.stop_gate_runner(&result_subject, true)?;
                self.record_once(
                    &result_subject,
                    "gate.result",
                    BTreeMap::from([
                        ("verdict".into(), Value::String("fail".into())),
                        (
                            "reason".into(),
                            Value::String(format!("LLM gate `{name}` exceeded {time_limit_ms}ms")),
                        ),
                        ("token_usage".into(), Value::from(0)),
                    ]),
                )?;
                return Ok(GateOutcome::Fail(format!(
                    "LLM gate `{name}` exceeded {time_limit_ms}ms"
                )));
            }
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let notify = self.notify.clone();
                let remaining = (time_limit_ms as u128).saturating_sub(elapsed) as u64;
                handle.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(remaining)).await;
                    notify.notify_one();
                });
            }
            return Ok(GateOutcome::Pending);
        }

        let (capability, capability_expires_at) =
            self.store
                .issue_capability("gate-result", &result_subject, None, time_limit_ms)?;
        self.record_once(
            &result_subject,
            "gate.requested",
            BTreeMap::from([
                ("status".into(), Value::String("requested".into())),
                ("model".into(), Value::String(model.into())),
                ("token_budget".into(), Value::from(token_budget)),
                (
                    "tools".into(),
                    Value::Array(tools.iter().cloned().map(Value::String).collect()),
                ),
                (
                    "capability_hash".into(),
                    Value::String(hex::encode(sha2::Sha256::digest(capability.as_bytes()))),
                ),
                (
                    "capability_expires_at".into(),
                    Value::String(capability_expires_at.to_string()),
                ),
            ]),
        )?;
        let instruction = format!(
            "{prompt}\n\nYou are a held-out st gate. Inspect only the declared workspace and tools. When you decide, run exactly one of these commands:\n  \"$ST3_BIN\" gate-result pass --reason 'REASON'\n  \"$ST3_BIN\" gate-result fail --reason 'REASON'\nDo not finish without posting a gate-result."
        );
        let argv = if model.starts_with("claude") {
            vec![
                "claude".into(),
                "-p".into(),
                "--model".into(),
                model.into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
                "--output-format".into(),
                "json".into(),
                instruction,
            ]
        } else {
            vec![
                "codex".into(),
                "exec".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--model".into(),
                model.into(),
                "--json".into(),
                instruction,
            ]
        };
        let mut environment = environment.clone();
        environment.insert("ST_GATE_SUBJECT".into(), result_subject.clone());
        environment.insert("ST_GATE_CAPABILITY".into(), capability);
        environment.insert("ST3_TOKEN_BUDGET".into(), token_budget.to_string());
        let member = MemberSpec {
            kind: MemberKind::Exec,
            host: host.into(),
            runtime_id: result_subject.replace('/', "."),
            workspace: workspace.into(),
            workspace_create: false,
            cwd: workspace.into(),
            terminal: false,
            launch: LaunchSpec::Argv(argv),
            environment,
            tags: BTreeMap::new(),
            display_name: None,
            lifecycle: MemberLifecycle::Service,
            restart: RestartType::Never,
            restart_intensity: RestartIntensity::default(),
            shutdown_timeout_ms: 5_000,
            driver: Some("llm-gate".into()),
        };
        let desired = DesiredSubject {
            subject: result_subject,
            kind: "gate".into(),
            desired: Value::Null,
            member: Some(member.clone()),
            owner_run: None,
            owner_generation: None,
            owner_step: None,
        };
        self.perform_start(&desired, &member, "the LLM gate was requested")?;
        Ok(GateOutcome::Pending)
    }

    fn stop_gate_runner(&self, subject: &str, hard: bool) -> Result<()> {
        let runtime_id = subject.replace('/', ".");
        let Some(observation) = self.runtime.observe_exec(&runtime_id)? else {
            return Ok(());
        };
        if observation.status != "running" {
            return Ok(());
        }
        let incarnation = observation.incarnation_id.as_deref();
        let action = if hard { "kill-gate" } else { "stop-gate" };
        if self
            .store
            .observations_for(subject, "runtime.action.succeeded")?
            .iter()
            .any(|claim| {
                claim.body.pointer("/fields/action").and_then(Value::as_str) == Some(action)
                    && claim
                        .body
                        .pointer("/fields/incarnation_id")
                        .and_then(Value::as_str)
                        == incarnation
            })
        {
            return Ok(());
        }
        let request = self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.requested".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String(action.into())),
                ("runtime_id".into(), Value::String(runtime_id.clone())),
                (
                    "incarnation_id".into(),
                    incarnation.map_or(Value::Null, |value| Value::String(value.into())),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "{action}:{subject}:{}",
                incarnation.unwrap_or("unknown")
            )),
        })?;
        if hard {
            self.runtime.kill(&runtime_id, false, incarnation)?;
        } else {
            self.runtime.stop(&runtime_id, false, incarnation)?;
        }
        self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: "runtime.action.succeeded".into(),
            actor: None,
            fields: BTreeMap::from([
                ("action".into(), Value::String(action.into())),
                (
                    "incarnation_id".into(),
                    incarnation.map_or(Value::Null, |value| Value::String(value.into())),
                ),
            ]),
            evidence: vec![request.id],
            expected_subject: None,
            idempotency_key: Some(format!(
                "{action}-complete:{subject}:{}",
                incarnation.unwrap_or("unknown")
            )),
        })?;
        Ok(())
    }

    fn subject_text(&self, subject: &str) -> Result<Option<String>> {
        if subject.starts_with("doc/") {
            if let Some((name, hash)) = subject.rsplit_once('@') {
                return self
                    .store
                    .get_document(name, hash)?
                    .map(|bytes| String::from_utf8(bytes).map_err(Into::into))
                    .transpose();
            }
            let Some(hash) = self.store.latest_document_hash(subject)? else {
                return Ok(None);
            };
            return self
                .store
                .get_document(subject, &hash)?
                .map(|bytes| String::from_utf8(bytes).map_err(Into::into))
                .transpose();
        }
        let Some(actual) = self.subject_value(subject)? else {
            return Ok(None);
        };
        if let Some(content) = actual_field(&actual, "content").and_then(Value::as_str) {
            return Ok(Some(content.into()));
        }
        let Some(hash) = actual_field(&actual, "blob_hash").and_then(Value::as_str) else {
            return Ok(None);
        };
        self.store
            .get_blob(hash)?
            .map(|bytes| String::from_utf8(bytes).map_err(Into::into))
            .transpose()
    }

    fn subject_value(&self, subject: &str) -> Result<Option<Value>> {
        if subject.starts_with("resource/")
            && let Some((name, claim_id)) = subject.rsplit_once('@')
        {
            return Ok(self
                .store
                .claim_by_id(claim_id)?
                .filter(|claim| claim.subject == name)
                .map(|claim| claim.body));
        }
        self.store.latest_actual_value(subject)
    }

    fn ensure_file_observation(&self, subject: &str) -> Result<()> {
        let Some(rest) = subject.strip_prefix("file/") else {
            return Ok(());
        };
        let Some((host, path)) = rest.split_once(':') else {
            return Ok(());
        };
        if host != self.host {
            return Ok(());
        }
        self.ensure_file_watch(subject, Path::new(path))?;
        let before = FileStamp::read(Path::new(path));
        if before.as_ref().is_some_and(|stamp| {
            self.file_observations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(subject)
                == Some(stamp)
        }) {
            return Ok(());
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                let blob_hash = self.store.put_blob(&bytes)?;
                let content_hash = hex::encode(sha2::Sha256::digest(&bytes));
                let after = FileStamp::read(Path::new(path));
                let mode = after.as_ref().map(|stamp| stamp.mode);
                let mut fields = BTreeMap::from([
                    ("status".into(), Value::String("observed".into())),
                    ("path".into(), Value::String(path.into())),
                    ("content_hash".into(), Value::String(content_hash)),
                    ("blob_hash".into(), Value::String(blob_hash)),
                    (
                        "content".into(),
                        Value::String(String::from_utf8_lossy(&bytes).into_owned()),
                    ),
                ]);
                if let Some(mode) = mode {
                    fields.insert("mode".into(), Value::from(mode));
                }
                self.record_once(subject, "file.observed", fields)?;
                let mut observations = self
                    .file_observations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if before == after {
                    if let Some(stamp) = after {
                        observations.insert(subject.into(), stamp);
                    }
                } else {
                    observations.remove(subject);
                }
            }
            Err(error) => {
                self.file_observations
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(subject);
                self.record_once(
                    subject,
                    "file.observed",
                    BTreeMap::from([
                        ("status".into(), Value::String("unreadable".into())),
                        ("path".into(), Value::String(path.into())),
                        ("reason".into(), Value::String(error.to_string())),
                    ]),
                )?;
            }
        }
        Ok(())
    }

    fn ensure_file_watch(&self, subject: &str, path: &Path) -> Result<()> {
        self.file_watchers_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(subject.into());
        let mut watchers = self
            .file_watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if watchers.contains_key(subject) {
            return Ok(());
        }
        let notify = self.notify.clone();
        let observations = self.file_observations.clone();
        let watched_subject = subject.to_owned();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    observations
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&watched_subject);
                    notify.notify_one();
                }
            })?;
        let watched = path
            .ancestors()
            .find(|candidate| candidate.exists())
            .unwrap_or(path);
        watcher.watch(watched, notify::RecursiveMode::NonRecursive)?;
        watchers.insert(subject.into(), watcher);
        Ok(())
    }

    fn release_unused_file_watchers(&self) {
        let used = std::mem::take(
            &mut *self
                .file_watchers_used
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.file_watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|subject, _| used.contains(subject));
        self.file_observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|subject, _| used.contains(subject));
    }
}

fn permanent_observation_error(code: &str) -> bool {
    matches!(
        code,
        "stale-observer-revision"
            | "invalid-resource-observation"
            | "invalid-claim-field"
            | "unknown-resource-field"
            | "immutable-resource-field"
            | "claim-cardinality"
    )
}

fn observer_unreachable_since(store: &Store, subject: &str) -> Result<Option<u128>> {
    let mut since = None;
    for claim in store.claims_for(subject, Some("observer.state"))? {
        let state = claim.body.pointer("/fields/state").and_then(Value::as_str);
        if state == Some("unreachable") {
            since.get_or_insert(claim.accepted_at_unix_ms);
        } else {
            since = None;
        }
    }
    Ok(since)
}

fn request_observer_attention(
    store: &Store,
    subject: &str,
    owner_run: Option<&str>,
    revision: &str,
    reason: &str,
) -> Result<()> {
    let reviewer = owner_run
        .and_then(|owner| store.mission_run(owner).ok().flatten())
        .map(|run| run.requester)
        .filter(|requester| requester.starts_with("person/"))
        .unwrap_or_else(|| "person/operator".into());
    let hash = hex::encode(sha2::Sha256::digest(
        format!("{subject}:{revision}").as_bytes(),
    ));
    store.request_attention(
        &format!("attention/github-observer-{}", &hash[..20]),
        &AttentionRequest {
            reviewer,
            title: "GitHub observer needs attention".into(),
            reason: format!("{subject} cannot observe its repository: {reason}"),
            severity: "error".into(),
            targets: vec![subject.into()],
            actor: "agent/st3/reconciler".into(),
            idempotency_key: format!("github-observer-attention:{hash}"),
        },
    )?;
    Ok(())
}

struct RuntimeStep<'a> {
    spec: &'a StepSpec,
    dependency_prefix: String,
    parent: Option<String>,
}

fn flatten_mission_steps(mission: &MissionSpec) -> Vec<RuntimeStep<'_>> {
    fn append<'a>(
        mission: &'a MissionSpec,
        dependency_prefix: String,
        parent: Option<String>,
        output: &mut Vec<RuntimeStep<'a>>,
    ) {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            output.push(RuntimeStep {
                spec: step,
                dependency_prefix: dependency_prefix.clone(),
                parent: parent.clone(),
            });
            if let Some(nested) = &step.nested_mission {
                append(
                    nested,
                    format!("{}/{}", step.path, nested.id),
                    Some(step.path.clone()),
                    output,
                );
            }
        }
    }
    let mut output = Vec::new();
    append(mission, String::new(), None, &mut output);
    output
}

fn step_run_selector(view: &crate::model::StepRunView) -> WorkSelector {
    if let Some(agent) = &view.assigned_to {
        WorkSelector::Assigned {
            agent: agent.clone(),
        }
    } else if !view.available_to.is_empty() {
        WorkSelector::Available {
            agents: view.available_to.clone(),
        }
    } else {
        WorkSelector::Agentless
    }
}

/// The full subject of the run that an `after-run` step waits for.
fn awaited_run(
    run: &MissionRunView,
    step: &RuntimeStep<'_>,
    view: &crate::model::StepRunView,
) -> Result<Option<String>> {
    let Some(after) = &step.spec.after_run else {
        return Ok(None);
    };
    let after = crate::mission::interpolate(after, &run_variables(run, step, view))?;
    Ok(Some(crate::mission::after_run_subject(&after)))
}

fn run_variables(
    run: &MissionRunView,
    step: &RuntimeStep<'_>,
    view: &crate::model::StepRunView,
) -> BTreeMap<String, String> {
    let parent_step_run = step
        .parent
        .as_ref()
        .map(|path| {
            format!(
                "step-run/{}/{}",
                run.generation
                    .strip_prefix("run-generation/")
                    .unwrap_or(&run.generation),
                path
            )
        })
        .or_else(|| run.parent_step_run.clone())
        .unwrap_or_default();
    let mut variables = BTreeMap::from([
        (
            "ST_MISSION".into(),
            run.mission
                .strip_prefix("mission/")
                .unwrap_or(&run.mission)
                .into(),
        ),
        ("ST_MISSION_REVISION".into(), run.revision.clone()),
        ("ST_MISSION_RUN".into(), run.id.clone()),
        (
            "ST_RUN_GENERATION".into(),
            run.generation
                .strip_prefix("run-generation/")
                .unwrap_or(&run.generation)
                .into(),
        ),
        ("ST_WORKSPACE".into(), run.workspace.clone()),
        ("ST_STEP".into(), step.spec.path.clone()),
        ("ST_STEP_RUN".into(), view.subject.clone()),
        ("ST_ATTEMPT".into(), view.attempt.to_string()),
        (
            "ST_ASSIGNEE".into(),
            view.assigned_to.clone().unwrap_or_default(),
        ),
        ("ST_REQUESTER".into(), run.requester.clone()),
        ("ST_PARENT_STEP_RUN".into(), parent_step_run),
        ("ST_ROOT_MISSION_RUN".into(), run.root_mission_run.clone()),
        (
            "ST_ROOT_MISSION_RUN_ID".into(),
            run.root_mission_run
                .strip_prefix("mission-run/")
                .unwrap_or(&run.root_mission_run)
                .into(),
        ),
        ("PATH".into(), "${PATH}".into()),
    ]);
    variables.extend(
        run.inputs
            .iter()
            .map(|(name, input)| (format!("input.{name}"), input.value.clone())),
    );
    if let Some(round) = run.inputs.get(LOOP_ROUND_INPUT) {
        variables.insert("ST_LOOP_ROUND".into(), round.value.clone());
        variables.insert("loop.round".into(), round.value.clone());
    }
    if let Some(feedback) = run.inputs.get(LOOP_FEEDBACK_INPUT) {
        variables.insert("ST_LOOP_FEEDBACK".into(), feedback.value.clone());
        variables.insert("loop.feedback".into(), feedback.value.clone());
    }
    if let Some(candidate) = run.inputs.get(CANDIDATE_INDEX_INPUT) {
        variables.insert("ST_CANDIDATE_INDEX".into(), candidate.value.clone());
        variables.insert("candidate.index".into(), candidate.value.clone());
    }
    if let Some(item) = run.inputs.get(LOOP_ITEM_INPUT)
        && let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&item.value)
    {
        for (name, value) in fields {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            variables.insert(format!("loop.item.{name}"), value);
        }
        let id = variables.get("loop.item.id").cloned().unwrap_or_default();
        variables.insert("ST_LOOP_ITEM_ID".into(), id);
    }
    variables
}

fn expand_gate(
    gate: &mut GateSpec,
    variables: &BTreeMap<String, String>,
    run_workspace: &str,
) -> Result<()> {
    let expand = |value: &mut String| -> Result<()> {
        *value = crate::mission::interpolate(value, variables)?;
        Ok(())
    };
    match gate {
        GateSpec::Exists { subject, .. } | GateSpec::Empty { subject, .. } => expand(subject)?,
        GateSpec::Field { subject, value, .. } => {
            expand(subject)?;
            if let Value::String(value) = value {
                expand(value)?;
            }
        }
        GateSpec::Every {
            subject, fields, ..
        }
        | GateSpec::NotEvery {
            subject, fields, ..
        } => {
            expand(subject)?;
            for field in fields {
                if let Value::String(value) = &mut field.value {
                    expand(value)?;
                }
            }
        }
        GateSpec::Has { subject, text, .. } | GateSpec::Lacks { subject, text, .. } => {
            expand(subject)?;
            expand(text)?;
        }
        GateSpec::Mechanical {
            name,
            command,
            host,
            workspace,
            environment,
            ..
        } => {
            expand(command)?;
            expand(host)?;
            expand(workspace)?;
            if Path::new(workspace).is_relative() {
                *workspace = Path::new(run_workspace)
                    .join(&*workspace)
                    .to_string_lossy()
                    .into_owned();
            }
            for value in environment.values_mut() {
                expand(value)?;
            }
            environment.extend(variables.clone());
            environment.insert("ST_GATE".into(), name.clone());
        }
        GateSpec::Llm {
            name,
            model,
            host,
            workspace,
            environment,
            prompt,
            ..
        } => {
            expand(model)?;
            expand(host)?;
            expand(workspace)?;
            expand(prompt)?;
            if Path::new(workspace).is_relative() {
                *workspace = Path::new(run_workspace)
                    .join(&*workspace)
                    .to_string_lossy()
                    .into_owned();
            }
            for value in environment.values_mut() {
                expand(value)?;
            }
            environment.extend(variables.clone());
            environment.insert("ST_GATE".into(), name.clone());
        }
        GateSpec::Human {
            reviewer,
            question,
            review_targets,
            ..
        } => {
            expand(reviewer)?;
            if let Some(question) = question {
                expand(question)?;
            }
            for target in review_targets {
                expand(target)?;
            }
        }
        GateSpec::Deadline { .. } => {}
    }
    Ok(())
}

/// Report whether an observer, subscription, or schedule declaration is a stop.
fn intake_is_stopped(subject: &DesiredSubject, host: &str) -> bool {
    match subject.kind.as_str() {
        "observer" => {
            crate::graph::observer_spec(&subject.desired).is_some_and(|spec| spec.stopped)
        }
        "subscription" => {
            crate::graph::subscription_spec(&subject.desired).is_some_and(|spec| spec.stopped)
        }
        "schedule" => {
            crate::graph::schedule_spec(&subject.desired, host).is_some_and(|spec| spec.stopped)
        }
        _ => false,
    }
}

/// Return the step path of a `step-run/GENERATION/PATH` subject.
fn step_path_of_subject(subject: &str) -> Option<&str> {
    subject
        .strip_prefix("step-run/")?
        .split_once('/')
        .map(|(_, path)| path)
}

/// Whether a mission run could not start only because this host lacks a claim that replication
/// can still deliver, such as a mission revision, an owner run, or an input's claim published on
/// another host.
fn start_waits_for_replication(error: &crate::model::St3Error) -> bool {
    matches!(
        error.code,
        "missing-mission" | "missing-mission-run" | "missing-resource-input-version" | "internal"
    )
}

/// Run `item`, turning a panic into an error so one item cannot end the reconciler task.
fn caught<T>(item: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(AssertUnwindSafe(item)).unwrap_or_else(|panic| {
        Err(anyhow::anyhow!(
            "panicked: {}",
            panic_message(panic.as_ref())
        ))
    })
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".into())
}

fn signal_changed(reconcile_notify: &Notify, event_notify: &watch::Sender<u64>) {
    reconcile_notify.notify_one();
    event_notify.send_modify(|generation| *generation = generation.saturating_add(1));
}

fn harness_incarnation_key(incarnation: &str) -> String {
    hex::encode(sha2::Sha256::digest(incarnation.as_bytes()))[..12].to_owned()
}

/// A declared product subject and the fields it must carry.
struct MissingProduct {
    subject: String,
    fields: Vec<(String, Value)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkWakeDecision {
    Wait,
    Request(u32),
    Exhaust,
}

fn work_wake_decision(
    attempts: u32,
    last_attempt_at_unix_ms: Option<u128>,
    first_attempt_at_unix_ms: Option<u128>,
    acknowledged: bool,
    now_unix_ms: u128,
) -> WorkWakeDecision {
    if acknowledged {
        return WorkWakeDecision::Wait;
    }
    let due = last_attempt_at_unix_ms
        .is_none_or(|last| now_unix_ms.saturating_sub(last) >= WORK_WAKE_RETRY_MS);
    if !due {
        WorkWakeDecision::Wait
    } else if attempts < WORK_WAKE_MAX_ATTEMPTS {
        WorkWakeDecision::Request(attempts.saturating_add(1))
    } else if first_attempt_at_unix_ms
        .is_none_or(|first| now_unix_ms.saturating_sub(first) < WORK_WAKE_EXHAUST_GRACE_MS)
    {
        WorkWakeDecision::Wait
    } else {
        WorkWakeDecision::Exhaust
    }
}

fn work_wake_acknowledged(
    attempts: &[(u128, &crate::model::MessageView)],
    harness: Option<&CurrentHarnessView>,
) -> bool {
    // A native read or close is durable evidence of a consumed wake even if a
    // short turn returned to ready before the harness observation caught it.
    attempts
        .iter()
        .any(|(_, message)| matches!(message.status.as_str(), "read" | "closed"))
        || attempts.first().is_some_and(|(requested, _)| {
            harness.is_some_and(|harness| {
                harness.state == "working" && harness.observed_at_unix_ms >= *requested
            })
        })
        // Pi-family drivers steer a wake into the turn that is already running, for example the
        // boot turn, and record delivery when the provider accepts it. That turn has consumed the
        // wake even though no new `working` edge follows. Another attempt would only interrupt it.
        || harness.is_some_and(|harness| harness.state == "working")
            && attempts
                .iter()
                .any(|(_, message)| message.status == "delivered")
        || harness.is_some_and(|harness| {
            harness.state == "working"
                && matches!(harness.driver.as_deref(), Some("pi" | "omp"))
        }) && attempts.iter().any(|(_, message)| message.status == "staged")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_work_wake_message(
    store: &Store,
    step: &StepRunView,
    agent: &str,
    incarnation: &str,
    wake_attempt: u32,
    source: &str,
    requested_by: &str,
    reason: &str,
    idempotency_key: String,
) -> Result<MessageView> {
    let incarnation_key = harness_incarnation_key(incarnation);
    let message_id = &hex::encode(sha2::Sha256::digest(idempotency_key.as_bytes()))[..16];
    let message_subject = format!("message/{message_id}");
    let queue = step
        .queue
        .as_deref()
        .zip(step.queue_position)
        .map(|(queue, position)| format!("\nQueue: {queue} #{position}"))
        .unwrap_or_default();
    let wake_description = if source == "manual" {
        format!("manual request ({reason})")
    } else {
        format!("{source} attempt {wake_attempt} ({reason})")
    };
    let content = format!(
        "A mission step is ready: {0}. Run `st work claim {0}` to read and claim it.\n\nTitle: {1}{queue}\nWake: {wake_description}",
        step.subject,
        step.title.as_deref().unwrap_or(&step.step),
    );
    let tag_value = format!(
        "{}@{}@{}@{}",
        step.subject, step.attempt, step.readiness_epoch, incarnation_key
    );
    store.append_claim(&ClaimInput {
        subject: message_subject.clone(),
        kind: "message.sent".into(),
        actor: Some("daemon/runtime".into()),
        fields: BTreeMap::from([
            ("from".into(), Value::String("daemon/runtime".into())),
            ("to".into(), Value::String(agent.into())),
            ("content".into(), Value::String(content.clone())),
            ("status".into(), Value::String("sent".into())),
            (
                "title".into(),
                Value::String(format!(
                    "Mission step ready: {}",
                    step.title.as_deref().unwrap_or(&step.step)
                )),
            ),
            ("in_reply_to".into(), Value::Null),
            (
                "tags".into(),
                Value::Array(vec![
                    Value::String(format!("st3-work:{tag_value}")),
                    Value::String(format!("mission-run:{}", step.run)),
                    Value::String(format!("st3-wake-attempt:{wake_attempt}")),
                    Value::String(format!("st3-wake-source:{source}")),
                    Value::String(format!("st3-wake-requested-by:{requested_by}")),
                ]),
            ),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(idempotency_key),
    })?;
    Ok(MessageView {
        subject: message_subject,
        from: "daemon/runtime".into(),
        to: agent.into(),
        content,
        status: "sent".into(),
        title: Some(format!(
            "Mission step ready: {}",
            step.title.as_deref().unwrap_or(&step.step)
        )),
        in_reply_to: None,
        tags: vec![
            format!("st3-work:{tag_value}"),
            format!("mission-run:{}", step.run),
            format!("st3-wake-attempt:{wake_attempt}"),
            format!("st3-wake-source:{source}"),
            format!("st3-wake-requested-by:{requested_by}"),
        ],
        created_index: store
            .latest_claim(&format!("message/{message_id}"), Some("message.sent"))?
            .map(|claim| claim.store_index)
            .unwrap_or_default(),
    })
}

fn message_sent_at(store: &Store, message: &MessageView) -> Option<u128> {
    store
        .claims_for(&message.subject, Some("message.sent"))
        .ok()?
        .into_iter()
        .next()
        .map(|claim| claim.accepted_at_unix_ms)
}

fn work_wake_attempt_evidence(
    store: &Store,
    attempts: &[(u128, &MessageView)],
) -> Result<Vec<String>> {
    Ok(attempts
        .iter()
        .map(|(_, message)| store.latest_claim(&message.subject, Some("message.sent")))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .map(|claim| claim.id)
        .collect())
}

fn work_wake_deadline(
    work: &[StepRunView],
    local_agents: &BTreeSet<String>,
    run_orders: &BTreeMap<String, Vec<String>>,
    now: u128,
) -> Option<u128> {
    let wakes = local_agents
        .iter()
        .filter_map(|agent| {
            let order = run_orders.get(agent).map(Vec::as_slice).unwrap_or_default();
            next_work_wake_for_agent(agent, work, order)
        })
        .collect::<BTreeSet<_>>();
    work.iter()
        .filter(|step| {
            step.assigned_to
                .as_ref()
                .is_some_and(|assignee| local_agents.contains(assignee))
        })
        .filter(|step| wakes.contains(step.subject.as_str()))
        .filter_map(|step| {
            let wake = step.wake.as_ref()?;
            if wake.assignee_state == "working" && nested_under_work(step, work) {
                return None;
            }
            Some(wake)
        })
        .filter(|wake| {
            matches!(wake.assignee_state.as_str(), "ready" | "working" | "idle")
                && wake.acknowledged_by.is_none()
                && wake.failure.is_none()
                && wake.attempts <= WORK_WAKE_MAX_ATTEMPTS
        })
        .map(|wake| {
            let delay = if wake.attempts == WORK_WAKE_MAX_ATTEMPTS {
                // Reconcile once more after startup grace to record a genuine
                // exhaustion even when no other graph event arrives.
                WORK_WAKE_EXHAUST_GRACE_MS
            } else {
                WORK_WAKE_RETRY_MS
            };
            wake.last_attempt_at_unix_ms
                .map_or(now, |last| last.saturating_add(delay))
        })
        .min()
}

/// How long to sleep before the next pass. A deadline that was already due when the last pass
/// began, and that pass changed nothing, cannot be acted on yet, so the loop backs off instead of
/// spinning. A deadline that fell due during or after that pass has not been evaluated, so it
/// runs at once: a mission timeout must not wait out the back-off.
fn deadline_sleep_ms(deadline: u128, now: u128, quiet_pass_started: Option<u128>) -> u64 {
    if quiet_pass_started.is_some_and(|started| deadline <= started) {
        WORK_WAKE_RETRY_MS as u64
    } else {
        deadline.saturating_sub(now).min(u128::from(u64::MAX)) as u64
    }
}

/// True when the seat holds nothing and has ready work in more than one run.
/// Only then can its run order change what it takes next.
fn seat_chooses_between_runs(agent: &str, work: &[StepRunView]) -> bool {
    let steps = work
        .iter()
        .map(crate::seat_queue::SeatStep::from)
        .collect::<Vec<_>>();
    let selection = crate::seat_queue::select(agent, &steps, &[]);
    if !selection.held.is_empty() {
        return false;
    }
    let mut runs = steps
        .iter()
        .filter(|step| selection.ready.contains(&step.subject))
        .map(|step| step.run);
    runs.next()
        .is_some_and(|first| runs.any(|run| run != first))
}

/// A maintained harness has one work seat. A claimed step occupies it, while
/// ready steps wait in the seat's ordered queue of mission runs.
fn next_work_wake_for_agent<'a>(
    agent: &str,
    work: &'a [StepRunView],
    run_order: &[String],
) -> Option<&'a str> {
    let steps = work
        .iter()
        .map(crate::seat_queue::SeatStep::from)
        .collect::<Vec<_>>();
    crate::seat_queue::select(agent, &steps, run_order).wake()
}

/// Inherited work is woken only after an early parent submission. A turn that is still working can
/// claim it without another message interrupting that turn; an idle seat gets the wake.
fn defers_inherited_work_wake(
    step: &StepRunView,
    work: &[StepRunView],
    harness: Option<&CurrentHarnessView>,
) -> bool {
    harness.is_some_and(|harness| harness.state == "working") && nested_under_work(step, work)
}

fn nested_under_work(step: &StepRunView, work: &[StepRunView]) -> bool {
    let nested = crate::seat_queue::SeatStep::from(step);
    work.iter().any(|ancestor| {
        crate::seat_queue::nests_under(&crate::seat_queue::SeatStep::from(ancestor), &nested)
    })
}

fn work_message_target(message: &crate::model::MessageView) -> Option<(&str, u32, u32, &str)> {
    message.tags.iter().find_map(|tag| {
        let mut parts = tag.strip_prefix("st3-work:")?.rsplitn(4, '@');
        let incarnation = parts.next()?;
        let readiness_epoch = parts.next()?.parse::<u32>().ok()?;
        let attempt = parts.next()?.parse::<u32>().ok()?;
        let step_subject = parts.next()?;
        Some((step_subject, attempt, readiness_epoch, incarnation))
    })
}

fn work_message_should_close(
    work: &[crate::model::StepRunView],
    step_subject: &str,
    attempt: u32,
    readiness_epoch: u32,
    message_incarnation: &str,
    current_incarnation: &str,
    turn_acknowledged: bool,
) -> bool {
    let current = work.iter().any(|step| {
        step.subject == step_subject
            && step.attempt == attempt
            && step.readiness_epoch == readiness_epoch
    });
    let acknowledged = work.iter().any(|step| {
        step.subject == step_subject
            && step.attempt == attempt
            && step.readiness_epoch == readiness_epoch
            && matches!(
                step.status.as_str(),
                "claimed" | "working" | "completed" | "failed" | "cancelled"
            )
    });
    !current || acknowledged || message_incarnation != current_incarnation || turn_acknowledged
}

fn structured_token_usage(log: &str) -> Option<u64> {
    let mut usages = Vec::new();
    let trimmed = log.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        collect_token_usage(&value, &mut usages);
    } else {
        for line in log.lines().map(str::trim).filter(|line| !line.is_empty()) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                collect_token_usage(&value, &mut usages);
            }
        }
    }
    usages.into_iter().max()
}

fn collect_token_usage(value: &Value, usages: &mut Vec<u64>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Object(usage)) = object.get("usage")
                && let Some(total) = token_usage_total(usage)
            {
                usages.push(total);
            }
            for nested in object.values() {
                collect_token_usage(nested, usages);
            }
        }
        Value::Array(values) => {
            for nested in values {
                collect_token_usage(nested, usages);
            }
        }
        _ => {}
    }
}

fn token_usage_total(usage: &serde_json::Map<String, Value>) -> Option<u64> {
    for key in ["total_tokens", "totalTokens"] {
        if let Some(total) = usage.get(key).and_then(Value::as_u64) {
            return Some(total);
        }
    }
    let keys = [
        "input_tokens",
        "inputTokens",
        "output_tokens",
        "outputTokens",
        "cache_creation_input_tokens",
        "cacheCreationInputTokens",
        "cache_read_input_tokens",
        "cacheReadInputTokens",
    ];
    let mut found = false;
    let total = keys.into_iter().fold(0_u64, |total, key| {
        usage
            .get(key)
            .and_then(Value::as_u64)
            .map_or(total, |value| {
                found = true;
                total.saturating_add(value)
            })
    });
    found.then_some(total)
}

enum GateOutcome {
    Pass,
    Pending,
    Fail(String),
}

fn human_review_failure_reason(decision: &crate::model::ClaimRecord) -> String {
    match decision
        .body
        .pointer("/fields/reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
    {
        Some(reason) => format!("the human reviewer rejected the work: {reason}"),
        None => "the human reviewer rejected the work".into(),
    }
}

enum LoopBranchOutcome {
    Pending,
    Completed,
    Failed(String),
}

enum LoopCandidateSelection {
    Pending,
    NoWinner,
    Winner(u32),
}

enum UsedMissionOutcome {
    Pending,
    Completed,
    Failed(String),
}

/// The st executable members launch with. A deploy installs the new binary before it restarts
/// the daemon, and in between Linux names this process's image `PATH (deleted)`. Launching that
/// name fails every start in the window and can hold a seat in a crash loop, so use the
/// replacement installed at the original path.
fn launch_executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    Ok(replaced_executable(&current).unwrap_or(current))
}

fn replaced_executable(current: &Path) -> Option<PathBuf> {
    let original = PathBuf::from(current.to_str()?.strip_suffix(" (deleted)")?);
    original.is_file().then_some(original)
}

fn member_fields(
    member: &MemberSpec,
    status: &str,
    incarnation_id: Option<&str>,
    adopted: bool,
) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::from([
        ("status".into(), Value::String(status.into())),
        (
            "runtime_id".into(),
            Value::String(member.runtime_id.clone()),
        ),
        ("terminal".into(), Value::Bool(member.terminal)),
        ("host".into(), Value::String(member.host.clone())),
        ("adopted".into(), Value::Bool(adopted)),
        (
            "shutdown_timeout_ms".into(),
            Value::from(member.shutdown_timeout_ms),
        ),
        ("reachability".into(), Value::String("reachable".into())),
        ("reason".into(), Value::Null),
    ]);
    if let Some(incarnation_id) = incarnation_id {
        fields.insert(
            "incarnation_id".into(),
            Value::String(incarnation_id.into()),
        );
    }
    fields
}

enum RestartDecision {
    Start,
    Wait { until: u128, reason: String },
    Fail { reason: String },
}

fn launch_in_restart_window(
    store_index: u64,
    accepted_at_unix_ms: u128,
    reset_index: u64,
    now: u128,
    interval_ms: u64,
    mode: &str,
) -> bool {
    store_index > reset_index
        && (mode != "fail" || accepted_at_unix_ms > now.saturating_sub(interval_ms as u128))
}

fn actual_field<'a>(actual: &'a Value, path: &str) -> Option<&'a Value> {
    let mut value = actual.get("fields").unwrap_or(actual);
    for segment in path.split('.') {
        value = value.get(segment)?;
    }
    Some(value)
}

fn owned_field(mut value: Value, path: &str) -> Option<Value> {
    for segment in path.split('.') {
        value = value.get(segment)?.clone();
    }
    Some(value)
}

fn observed_field_value(actual: &Value, subject: &str, path: &str) -> Option<Value> {
    if subject.starts_with("file/") {
        actual_field(actual, "content")
            .and_then(Value::as_str)
            .and_then(|content| serde_json::from_str::<Value>(content).ok())
            .and_then(|content| owned_field(content, path))
    } else {
        let actual = if subject.starts_with("resource/") {
            actual.get("facts").unwrap_or(actual)
        } else {
            actual
        };
        actual_field(actual, path).cloned()
    }
}

fn compare_value(found: &Value, operator: &str, expected: &Value) -> bool {
    match operator {
        "is" => found == expected,
        "starts-with" => found
            .as_str()
            .zip(expected.as_str())
            .is_some_and(|(found, expected)| found.starts_with(expected)),
        "contains" => match (found, expected) {
            (Value::String(found), Value::String(expected)) => found.contains(expected),
            (Value::Array(found), expected) => found.contains(expected),
            _ => false,
        },
        _ => false,
    }
}

fn gate_operation_subject(stage: &GateContext, name: &str, definition: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(&(stage.subject.as_str(), name, definition))?;
    let hash = hex::encode(sha2::Sha256::digest(bytes));
    Ok(format!(
        "gate-operation/{}/{}",
        stage.subject.replace('/', "."),
        &hash[..24]
    ))
}

/// A retried attempt runs its mechanical and LLM gates again instead of reusing the first
/// attempt's result. The first attempt keeps its original key, so recorded results stay valid.
fn gate_result_subject(stage: &GateContext, name: &str, definition: &Value) -> Result<String> {
    if stage.attempt <= 1 {
        return gate_operation_subject(stage, name, definition);
    }
    let mut definition = definition.clone();
    definition["attempt"] = stage.attempt.into();
    gate_operation_subject(stage, name, &definition)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_exec_and_gate_shell_resolve_the_declared_path() {
        use super::{NativeRuntime, RuntimeControl};
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("orchid-tool");
        std::fs::write(&program, "#!/bin/sh\nprintf '%s' \"$ORCHID_VALUE\"\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let intent = crate::graph::parse_test_intent(
            &format!(
                r#"version 2
exec "orchid" {{ command "orchid-tool"; workspace "{}"; }}"#,
                root.path().display()
            ),
            "orchid",
        )
        .unwrap();
        let mut member = intent
            .subjects
            .values()
            .find_map(|subject| subject.member.clone())
            .unwrap();
        member.environment.insert(
            "PATH".into(),
            format!("{}:${{PATH}}", root.path().display()),
        );
        member
            .environment
            .insert("ORCHID_VALUE".into(), "from-declaration".into());
        let runtime = NativeRuntime::new(root.path(), None, std::path::Path::new("unused-pty"));
        for (id, launch) in [
            (
                "orchid-argv",
                crate::model::LaunchSpec::Argv(vec!["orchid-tool".into()]),
            ),
            (
                "orchid-shell",
                crate::model::LaunchSpec::Shell("orchid-tool".into()),
            ),
        ] {
            member.runtime_id = id.into();
            member.launch = launch;
            runtime.start(&member).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if runtime
                    .observe_exec(id)
                    .unwrap()
                    .is_some_and(|observation| observation.status == "exited")
                {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert_eq!(
                runtime.read_exec_log(id).unwrap().unwrap(),
                "from-declaration"
            );
        }
    }

    #[test]
    fn claude_login_screen_recognizes_only_explicit_auth_prompts() {
        assert_eq!(
            super::claude_login_expired("● Login expired · Please run /login"),
            Some("● Login expired · Please run /login")
        );
        assert_eq!(
            super::claude_login_expired("> \n  ⎿  Not logged in · Run /login  \n"),
            Some("⎿  Not logged in · Run /login")
        );
        assert_eq!(
            super::claude_login_expired("Please use /login to switch accounts"),
            None
        );
    }

    #[test]
    fn claude_login_screen_ignores_quoted_source_and_grep_output() {
        for screen in [
            r#"37:    screen.contains("Login expired · Please run /login")"#,
            r#"  ⎿  37:    screen.contains("Login expired · Please run /login")"#,
            r#"● The detector matches "Not logged in · Run /login" anywhere."#,
            "fn claude_login_expired(screen: &str) -> bool { // Login expired · Please run /login",
        ] {
            assert_eq!(super::claude_login_expired(screen), None, "{screen}");
        }
    }

    /// Claude Code 2.1.283's trust dialog as `pty peek --plain` rendered it at 80 columns.
    const CLAUDE_TRUST_SCREEN: &str = "\
────────────────────────────────────────────────────────────────────────────────
 Accessing workspace:

 /home/example/src/demo-project--feature-branch-with-a-long-worktree-name/nested
 /workspace

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel
";

    #[test]
    fn claude_trust_screen_recognizes_only_the_whole_dialog() {
        assert!(super::claude_trust_prompt(CLAUDE_TRUST_SCREEN));
        // The question wraps at a narrower terminal width.
        assert!(super::claude_trust_prompt(&CLAUDE_TRUST_SCREEN.replace(
            "one you trust? (Like your\n own code",
            "one\n you trust? (Like\n your own code"
        )));
        assert!(!super::claude_trust_prompt(
            "> Why did the seat stop at \"Yes, I trust this folder\"?"
        ));
        assert!(!super::claude_trust_prompt("╭─ Claude Code ─╮\n> "));
    }
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{SecondsFormat, Utc};

    use super::*;
    use crate::graph::parse_test_intent as parse_intent;

    #[cfg(unix)]
    #[test]
    fn a_running_pty_with_a_dead_pid_is_observed_as_vanished() {
        let observation = st_runtime::PtyObservation {
            name: "stale-worker".into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(i32::MAX as u32),
            created_at: Some("2026-09-22T12:00:00.000Z".into()),
            display_name: None,
            tags: BTreeMap::new(),
        };

        assert_eq!(observed_pty_status(&observation), "vanished");
    }

    #[cfg(unix)]
    #[test]
    fn a_running_pty_with_a_live_pid_remains_running() {
        let observation = st_runtime::PtyObservation {
            name: "live-worker".into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(std::process::id()),
            created_at: Some("2026-09-22T12:00:00.000Z".into()),
            display_name: None,
            tags: BTreeMap::new(),
        };

        assert_eq!(observed_pty_status(&observation), "running");
    }

    #[derive(Default)]
    struct FakeRuntime {
        snapshot_error: Mutex<bool>,
        ptys: Mutex<Vec<RuntimeObservation>>,
        execs: Mutex<HashMap<String, RuntimeObservation>>,
        logs: Mutex<HashMap<String, String>>,
        starts: Mutex<Vec<String>>,
        failed_starts: Mutex<std::collections::HashSet<String>>,
        failed_observes: Mutex<std::collections::HashSet<String>>,
        failed_stops: Mutex<std::collections::HashSet<String>>,
        started_members: Mutex<Vec<MemberSpec>>,
        stops: Mutex<Vec<String>>,
        kills: Mutex<Vec<String>>,
        removes: Mutex<Vec<String>>,
        screen: Mutex<String>,
        screens: Mutex<HashMap<String, String>>,
        keys: Mutex<Vec<String>>,
    }

    impl RuntimeControl for FakeRuntime {
        fn snapshot_ptys(&self) -> Result<Vec<RuntimeObservation>> {
            if *self.snapshot_error.lock().unwrap() {
                anyhow::bail!("the PTY snapshot is unavailable")
            }
            Ok(self.ptys.lock().unwrap().clone())
        }
        fn observe_exec(&self, runtime_id: &str) -> Result<Option<RuntimeObservation>> {
            anyhow::ensure!(
                !self.failed_observes.lock().unwrap().contains(runtime_id),
                "fake observe failed"
            );
            Ok(self.execs.lock().unwrap().get(runtime_id).cloned())
        }
        fn start(&self, member: &MemberSpec) -> Result<()> {
            self.starts.lock().unwrap().push(member.runtime_id.clone());
            if self
                .failed_starts
                .lock()
                .unwrap()
                .contains(&member.runtime_id)
            {
                anyhow::bail!("the fake runtime rejected the start")
            }
            self.started_members.lock().unwrap().push(member.clone());
            Ok(())
        }
        fn stop(
            &self,
            runtime_id: &str,
            _terminal: bool,
            _expected_incarnation: Option<&str>,
        ) -> Result<()> {
            self.stops.lock().unwrap().push(runtime_id.into());
            anyhow::ensure!(
                !self.failed_stops.lock().unwrap().contains(runtime_id),
                "fake stop failed"
            );
            Ok(())
        }
        fn kill(
            &self,
            runtime_id: &str,
            _terminal: bool,
            _expected_incarnation: Option<&str>,
        ) -> Result<()> {
            self.kills.lock().unwrap().push(runtime_id.into());
            Ok(())
        }
        fn remove(&self, runtime_id: &str, terminal: bool) -> Result<()> {
            self.removes.lock().unwrap().push(runtime_id.into());
            if terminal {
                self.ptys
                    .lock()
                    .unwrap()
                    .retain(|runtime| runtime.runtime_id != runtime_id);
            } else {
                self.execs.lock().unwrap().remove(runtime_id);
            }
            Ok(())
        }
        fn attach(&self, _runtime_id: &str) -> Result<()> {
            Ok(())
        }
        fn screen(&self, runtime_id: &str) -> Result<String> {
            let screens = self.screens.lock().unwrap();
            Ok(screens
                .get(runtime_id)
                .cloned()
                .unwrap_or_else(|| self.screen.lock().unwrap().clone()))
        }
        fn send_key(&self, _runtime_id: &str, key: &str) -> Result<()> {
            self.keys.lock().unwrap().push(key.into());
            Ok(())
        }
        fn read_exec_log(&self, runtime_id: &str) -> Result<Option<String>> {
            Ok(self.logs.lock().unwrap().get(runtime_id).cloned())
        }
    }

    #[tokio::test]
    async fn active_gates_share_a_slow_bounded_reconcile_poll() {
        let notify = Arc::new(Notify::new());
        let reconciler = Reconciler::new(
            Arc::new(Store::open_memory("node").unwrap()),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            notify.clone(),
        );

        // Several pending gates must not restart a full host pass every two seconds.
        for _ in 0..10 {
            reconciler.arm_gate_poll();
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(4), notify.notified())
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(8), notify.notified())
            .await
            .expect("the gate poll should eventually wake reconciliation");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .is_err()
        );
    }

    #[test]
    fn quantified_field_predicates_wait_for_the_last_item_and_evaluate_negation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let fields = vec![
            crate::model::QuantifiedFieldSpec {
                path: "status".into(),
                operator: "is".into(),
                value: Value::String("completed".into()),
            },
            crate::model::QuantifiedFieldSpec {
                path: "conclusion".into(),
                operator: "is".into(),
                value: Value::String("success".into()),
            },
        ];
        let every = GateSpec::Every {
            name: "all checks passed".into(),
            path: "checks".into(),
            subject: "resource/pull".into(),
            fields: fields.clone(),
        };
        let not_every = GateSpec::NotEvery {
            name: "a check did not pass".into(),
            path: "checks".into(),
            subject: "resource/pull".into(),
            fields,
        };
        let stage = GateContext {
            subject: "step-run/test/checks".into(),
            name: "checks".into(),
            started_at_unix_ms: now_ms(),
            attempt: 1,
        };

        assert!(matches!(
            reconciler.evaluate_gate(&stage, &every).unwrap(),
            GateOutcome::Pending
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &not_every).unwrap(),
            GateOutcome::Pending
        ));

        let observe = |checks: Value, key: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: "resource/pull".into(),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("kind".into(), Value::String("vcs.pull-request".into())),
                        ("facts".into(), serde_json::json!({"checks": checks})),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        };

        observe(
            serde_json::json!([
                {"status": "completed", "conclusion": "success"},
                {"status": "in_progress", "conclusion": null}
            ]),
            "one-check-pending",
        );
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &every).unwrap(),
            GateOutcome::Pending
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &not_every).unwrap(),
            GateOutcome::Pass
        ));

        observe(
            serde_json::json!([
                {"status": "completed", "conclusion": "success"},
                {"status": "completed", "conclusion": "success"}
            ]),
            "all-pass",
        );
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &every).unwrap(),
            GateOutcome::Pass
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &not_every).unwrap(),
            GateOutcome::Pending
        ));

        observe(
            serde_json::json!([
                {"status": "completed", "conclusion": "success"},
                {"status": "completed", "conclusion": "failure"}
            ]),
            "one-failed",
        );
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &every).unwrap(),
            GateOutcome::Pending
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &not_every).unwrap(),
            GateOutcome::Pass
        ));

        observe(Value::Array(Vec::new()), "empty-list");
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &every).unwrap(),
            GateOutcome::Pass
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &not_every).unwrap(),
            GateOutcome::Pending
        ));

        store
            .append_claim(&ClaimInput {
                subject: "resource/wrong".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("custom.st3.test".into())),
                    ("facts".into(), serde_json::json!({"checks": "not-a-list"})),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("wrong-shape".into()),
            })
            .unwrap();
        let mut wrong_every = every.clone();
        let GateSpec::Every { subject, .. } = &mut wrong_every else {
            unreachable!()
        };
        *subject = "resource/wrong".into();
        let mut wrong_not_every = not_every.clone();
        let GateSpec::NotEvery { subject, .. } = &mut wrong_not_every else {
            unreachable!()
        };
        *subject = "resource/wrong".into();
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &wrong_every).unwrap(),
            GateOutcome::Pending
        ));
        assert!(matches!(
            reconciler.evaluate_gate(&stage, &wrong_not_every).unwrap(),
            GateOutcome::Pending
        ));
    }

    fn apply_source(store: &Store, source: &str, idempotency_key: &str) {
        let intent = parse_intent(source, "node").unwrap();
        let mission = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &mission.subject_tokens, idempotency_key)
            .unwrap();
    }

    /// The in-memory store's connections share one cache. There a call that meets a pass of the
    /// running reconciler loop, which runs on the blocking pool, fails at once with `database
    /// table is locked`, where a daemon's file store would wait. A test beside the running loop
    /// retries such a call.
    fn beside_the_loop<T, E: std::fmt::Display>(mut call: impl FnMut() -> Result<T, E>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match call() {
                Ok(value) => return value,
                Err(error)
                    if error.to_string().contains("database table is locked")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    fn apply_source_beside_the_loop(store: &Store, source: &str, idempotency_key: &str) {
        let intent = parse_intent(source, "node").unwrap();
        let mission = beside_the_loop(|| {
            store.mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
        });
        beside_the_loop(|| store.apply(&intent, &mission.subject_tokens, idempotency_key));
    }

    #[test]
    fn reads_claude_structured_token_usage() {
        let log = r#"{"type":"result","usage":{"input_tokens":120,"cache_creation_input_tokens":30,"cache_read_input_tokens":40,"output_tokens":10}}"#;
        assert_eq!(structured_token_usage(log), Some(200));
    }

    #[test]
    fn reads_codex_jsonl_token_usage() {
        let log = concat!(
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":120,\"cached_input_tokens\":80,\"output_tokens\":30}}\n",
        );
        assert_eq!(structured_token_usage(log), Some(150));
    }

    #[test]
    fn a_mission_run_executes_parallel_roots_and_an_all_of_join() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "dag" state="ready" {
    goal "Complete mission dag."
    completion { when "all-steps-exhausted" }
    step "one" { }
    step "two" { }
    step "join" {
      depends-on {
        step "one" completed
        step "two" completed
      }
    }
  }

"#;
        apply_source(&store, source, "publish-dag");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "dag".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-dag".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, "completed");
        assert!(run.steps.iter().all(|step| step.status == "completed"));
    }

    #[test]
    fn mission_and_step_baselines_block_before_work_and_recheck_retries() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "baseline" state="ready" {
                goal "Run only from an admitted baseline."
                completion { when "all-steps-exhausted" }

                  agent "worker" { workspace "/tmp"; command "true"; restart "never" }

                baseline "the release is open" {
                  field "state" "resource/release" is "open"
                }
                step "work" {
                  baseline "the source is clean" {
                    field "state" "resource/source" is "clean"
                  }
                  retry { attempts 2 }
                }
              }

        "#;
        apply_source(&store, source, "baseline-mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "baseline".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "baseline-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        let blocked = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(blocked.status, "blocked");
        assert_eq!(blocked.steps[0].status, "pending");
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|subject| subject.subject != format!("agent/{}/worker", run.id))
        );

        for (subject, state, key) in [
            ("resource/release", "open", "release-open"),
            ("resource/source", "clean", "source-clean"),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("kind".into(), Value::String("custom.st3.state".into())),
                        ("state".into(), Value::String(state.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        }
        reconciler.reconcile_once().unwrap();
        let admitted = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(admitted.status, "running");
        assert_eq!(admitted.steps[0].status, "ready");
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .any(|subject| subject.subject == format!("agent/{}/worker", run.id))
        );

        store
            .append_claim(&ClaimInput {
                subject: "resource/release".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), Value::String("closed".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("release-closed-after-admission".into()),
            })
            .unwrap();
        store
            .set_step_state(&admitted.steps[0].subject, "failed", Some("test failure"))
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "resource/source".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), Value::String("dirty".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("source-dirty".into()),
            })
            .unwrap();
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let retried = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(retried.status, "running");
        assert_eq!(retried.steps[0].attempt, 2);
        assert_eq!(retried.steps[0].status, "blocked");
        assert!(
            retried.steps[0]
                .blocked_reason
                .as_deref()
                .unwrap()
                .contains("source is clean")
        );
    }

    #[test]
    fn mission_products_and_gates_hold_completion_and_record_evidence() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "release" state="ready" {
                goal "Publish an approved result."
                completion { when "all-steps-exhausted" }
                produces {
                  resource "result" { kind "custom.st3.release-result"; state "published" }
                }
                gate "the result is approved" {
                  field "approval" "resource/result" is "yes"
                }
                step "work" { }
              }

        "#;
        apply_source(&store, source, "release-mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "release".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "release-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let waiting = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(waiting.steps[0].status, "completed");
        assert_eq!(waiting.status, "running");

        store
            .append_claim(&ClaimInput {
                subject: "resource/result".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    (
                        "kind".into(),
                        Value::String("custom.st3.release-result".into()),
                    ),
                    ("state".into(), Value::String("published".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("result-published".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "running"
        );

        store
            .append_claim(&ClaimInput {
                subject: "resource/result".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("state".into(), Value::String("published".into())),
                    ("approval".into(), Value::String("yes".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("result-approved".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let completed = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        let evidence = store
            .claims_page(None, None, 0, None, false, 500)
            .unwrap()
            .claims
            .into_iter()
            .find(|claim| {
                claim.kind == "gate.result"
                    && claim.body.pointer("/fields/gate").and_then(Value::as_str)
                        == Some("the result is approved")
            })
            .expect("the gate did not record evidence");
        assert_eq!(
            evidence
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("pass")
        );
    }

    #[test]
    fn step_members_and_gates_receive_automatic_st_context() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "context" state="ready" {
                goal "Expose the run context."

                  exec "mission-task" {
                    command "true"
                    env { CUSTOM_PATH "/opt/st3-shims:${PATH}" }
                  }

                step "work" {

                    exec "task" {
                      command "true"
                      env { CUSTOM_RUN "${ST_MISSION_RUN}" }
                    }

                  gate "verify context" {
                    exec "true"
                    host "node"
                    workspace "."
                    time-limit "1m"
                  }
                }
              }

        "#;
        apply_source(&store, source, "context-mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "context".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "context-run".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let task = runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|member| member.environment.contains_key("CUSTOM_RUN"))
            .cloned()
            .expect("the step task did not start");
        for name in [
            "ST_MISSION",
            "ST_MISSION_REVISION",
            "ST_MISSION_RUN",
            "ST_RUN_GENERATION",
            "ST_ROOT_MISSION_RUN",
            "ST_WORKSPACE",
            "ST_REQUESTER",
            "ST_STEP",
            "ST_STEP_RUN",
            "ST_ATTEMPT",
            "ST_ASSIGNEE",
            "ST_PARENT_STEP_RUN",
        ] {
            assert!(task.environment.contains_key(name), "missing {name}");
        }
        assert!(!task.environment.contains_key("ST_AGENT"));
        assert_eq!(task.environment["CUSTOM_RUN"], run.id);
        let mission_task = runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|member| member.environment.contains_key("CUSTOM_PATH"))
            .cloned()
            .expect("the mission task did not start");
        assert_eq!(
            mission_task.environment["CUSTOM_PATH"],
            "/opt/st3-shims:${PATH}"
        );

        runtime.execs.lock().unwrap().insert(
            task.runtime_id.clone(),
            RuntimeObservation {
                runtime_id: task.runtime_id,
                terminal: false,
                status: "exited".into(),
                exit_code: Some(0),
                incarnation_id: Some("task-one".into()),
            },
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let gate = runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|member| member.driver.as_deref() == Some("mechanical-gate"))
            .cloned()
            .expect("the mechanical gate did not start");
        assert_eq!(gate.environment["ST_GATE"], "verify context");
        assert_eq!(gate.environment["ST_MISSION_RUN"], run.id);
        assert_eq!(gate.environment["ST_STEP"], "work");
    }

    #[tokio::test]
    async fn materialized_step_declarations_wake_member_reconciliation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "wake" state="ready" {
    goal "Complete mission wake."
    step "team" {

        agent "worker" { workspace "/tmp"; command "true"; restart "never" }

    }
  }

"#;
        apply_source(&store, source, "publish-wake");
        store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "wake".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-wake".into(),
            })
            .unwrap();
        let notify = Arc::new(Notify::new());
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            notify.clone(),
        );
        reconciler.reconcile_once().unwrap();
        notify.notified().await;
        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified())
            .await
            .expect("the materialized declarations did not request another reconcile pass");
    }

    #[tokio::test]
    async fn completed_nested_work_wakes_its_successor_without_an_external_publish() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "worker" { workspace "/tmp"; command "true" }

mission "nested-wake" state="ready" {
  goal "Complete nested work."
  step "outer" {
    assigned-to "agent/worker"
    mission "work" {
      goal "Complete the child work."
      step "first" { }
      step "second" { depends-on { step "first" completed } }
    }
  }
}
"#;
        apply_source(&store, source, "nested-wake-source");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "nested-wake".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "nested-wake-run".into(),
            })
            .unwrap();
        let notify = Arc::new(Notify::new());
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            notify.clone(),
        );
        reconciler.reconcile_once().unwrap();
        let runtime_id = runtime.started_members.lock().unwrap()[0]
            .runtime_id
            .clone();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id,
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("current".into()),
        });
        reconciler.reconcile_once().unwrap();
        let run = store.mission_run(&run.id).unwrap().unwrap();
        let parent = run.steps.iter().find(|step| step.step == "outer").unwrap();
        let first = run
            .steps
            .iter()
            .find(|step| step.step == "outer/work/first")
            .unwrap();
        let second = run
            .steps
            .iter()
            .find(|step| step.step == "outer/work/second")
            .unwrap();
        let request = |key: &str| crate::model::WorkRequest {
            actor: Some("agent/node.worker".into()),
            incarnation: Some("current".into()),
            summary: None,
            reason: None,
            evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        store
            .work_action(&parent.subject, "claim", &request("claim-parent"))
            .unwrap();
        store
            .work_action(&parent.subject, "progress", &request("progress-parent"))
            .unwrap();
        reconciler.reconcile_once().unwrap();
        store
            .work_action(&first.subject, "claim", &request("claim-first"))
            .unwrap();
        store
            .work_action(&first.subject, "complete", &request("complete-first"))
            .unwrap();
        while tokio::time::timeout(std::time::Duration::from_millis(1), notify.notified())
            .await
            .is_ok()
        {}

        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(50), notify.notified())
            .await
            .expect("the completed child did not request another reconcile pass");
        reconciler.reconcile_once().unwrap();

        let parent = store.step_run(&parent.subject).unwrap().unwrap();
        let first = store.step_run(&first.subject).unwrap().unwrap();
        let second = store.step_run(&second.subject).unwrap().unwrap();
        assert_eq!(parent.status, "working");
        assert_eq!(parent.claimant.as_deref(), Some("agent/node.worker"));
        assert_eq!(first.status, "completed");
        assert_eq!(second.status, "ready");
    }

    #[test]
    fn only_the_mission_run_origin_materializes_its_members() {
        let source = Store::open_memory("source").unwrap();
        let kdl = r#"
version 2

  mission "origin-owned" state="ready" {
    goal "Run work on the origin node."
    step "team" {
      agent "worker" { workspace "/tmp"; command "true"; restart "never" }
    }
  }

"#;
        let intent = parse_intent(kdl, "source").unwrap();
        let planned = source
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
            )
            .unwrap();
        source
            .apply(&intent, &planned.subject_tokens, "publish-origin-owned")
            .unwrap();
        let run = source
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "origin-owned".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-origin-owned".into(),
            })
            .unwrap();
        assert_eq!(
            source.mission_run_origin(&run.id).unwrap().as_deref(),
            Some("source")
        );

        let replica = Arc::new(Store::open_memory("replica").unwrap());
        replica
            .import_replication("source", &source.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            replica.mission_run_origin(&run.id).unwrap().as_deref(),
            Some("source")
        );
        let replica_runtime = Arc::new(FakeRuntime::default());
        let replica_reconciler = Reconciler::new(
            replica.clone(),
            replica_runtime.clone(),
            "replica".into(),
            Arc::new(Notify::new()),
        );
        replica_reconciler.reconcile_once().unwrap();
        assert!(replica.desired_subjects().unwrap().is_empty());
        assert!(replica_runtime.started_members.lock().unwrap().is_empty());

        let source = Arc::new(source);
        let source_reconciler = Reconciler::new(
            source.clone(),
            Arc::new(FakeRuntime::default()),
            "source".into(),
            Arc::new(Notify::new()),
        );
        source_reconciler.reconcile_once().unwrap();
        source_reconciler.reconcile_once().unwrap();
        assert!(
            source
                .desired_subjects()
                .unwrap()
                .iter()
                .any(|subject| subject.owner_run.as_deref() == Some(run.subject.as_str()))
        );
    }

    #[test]
    fn one_failed_runtime_start_does_not_block_other_runtime_starts() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "start-failure" state="ready" {
    goal "Start all independent mission members."
    step "team" {

        agent "bad" { workspace "/tmp"; command "true"; restart "never" }
        agent "good" { workspace "/tmp"; command "true"; restart "never" }

    }

  }

"#;
        apply_source(&store, source, "publish-start-failure");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "start-failure".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-start-failure".into(),
            })
            .unwrap();
        let bad_runtime = format!("{}.bad", run.id);
        let good_runtime = format!("{}.good", run.id);
        let bad_subject = format!("agent/{}/bad", run.id);
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_starts
            .lock()
            .unwrap()
            .insert(bad_runtime.clone());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }

        let starts = runtime.starts.lock().unwrap();
        assert!(starts.contains(&bad_runtime));
        assert!(starts.contains(&good_runtime));
        assert!(
            store
                .latest_observation(&bad_subject, "runtime.action.failed")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn failed_durable_start_waits_instead_of_relaunching_each_pass() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            "version 2\nagent \"bad\" { workspace \"/tmp\"; command \"true\"; restart \"always\" }\n",
            "failed-durable-start",
        );
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_starts
            .lock()
            .unwrap()
            .insert("node.bad".into());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        assert!(
            reconciler
                .next_reconcile_deadline()
                .is_some_and(|deadline| deadline > now_ms())
        );
    }

    #[test]
    fn every_failed_member_start_waits_instead_of_relaunching_each_pass() {
        for (source, runtime_id) in [
            (
                "version 2\nexec \"bad\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\n",
                "exec.bad",
            ),
            (
                "version 2\nagent \"bad\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\n",
                "node.bad",
            ),
        ] {
            let store = Arc::new(Store::open_memory("node").unwrap());
            apply_source(&store, source, "failed-start");
            let runtime = Arc::new(FakeRuntime::default());
            runtime
                .failed_starts
                .lock()
                .unwrap()
                .insert(runtime_id.into());
            let reconciler = Reconciler::new(
                store.clone(),
                runtime.clone(),
                "node".into(),
                Arc::new(Notify::new()),
            );
            for _ in 0..5 {
                reconciler.reconcile_once().unwrap();
            }
            assert_eq!(runtime.starts.lock().unwrap().len(), 1, "{source}");
            assert!(
                reconciler
                    .next_reconcile_deadline()
                    .is_some_and(|deadline| deadline > now_ms()),
                "{source}"
            );
        }
    }

    /// Fails every item of one scope.
    struct FailScope(&'static str);

    impl FaultInjection for FailScope {
        fn fault(&self, scope: &str, _subject: &str) -> Option<String> {
            (scope == self.0).then(|| format!("injected fault in {scope}"))
        }
    }

    #[test]
    fn a_deadline_source_that_fails_loses_only_its_own_deadlines() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_fault_injection(Arc::new(FailScope("deadline/work-wakes")));
        let restart = now_ms() + 1_000;
        reconciler
            .delayed_restarts
            .lock()
            .unwrap()
            .insert("agent/sample".into(), restart);

        let deadline = reconciler
            .next_reconcile_deadline()
            .expect("the failing source took the other deadlines with it");
        assert!(deadline <= restart);
        assert_eq!(
            store
                .reconcile_fault("daemon/node", "deadline/work-wakes")
                .unwrap()
                .as_deref(),
            Some("injected fault in deadline/work-wakes")
        );

        // Alone, the failing source is read again within a few seconds, not every second.
        reconciler.delayed_restarts.lock().unwrap().clear();
        let before = now_ms();
        let retry = reconciler
            .next_reconcile_deadline()
            .expect("a failing source asked for no retry");
        assert!(retry >= before + DEADLINE_SOURCE_RETRY_MS);
        assert!(retry <= now_ms() + DEADLINE_SOURCE_RETRY_MS);
    }

    #[test]
    fn a_member_declaration_this_build_cannot_read_is_faulted_on_itself() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            "version 2\nexec \"odd\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\nexec \"fine\" { workspace \"/tmp\"; command \"true\"; restart \"never\" }\n",
            "unreadable-member",
        );
        // A newer build's member, as an older build that replicated it would see it.
        store.replace_desired_member_for_test("exec/odd", r#"{"kind":"exec","sandbox":"strict"}"#);
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        assert_eq!(
            *runtime.starts.lock().unwrap(),
            vec!["exec.fine".to_owned()]
        );
        let fault = store
            .member_reconcile_fault("exec/odd", None)
            .unwrap()
            .expect("the unreadable declaration was skipped without a fault");
        assert!(
            fault.starts_with("this build cannot read the member declaration: "),
            "{fault}"
        );
    }

    #[test]
    fn a_render_warning_never_faults_an_exec_member() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        apply_source(
            &store,
            &format!(
                "version 2\nexec \"task\" {{ workspace {:?}; command \"true\"; render {{ git-exclude \".st3/\" }} }}\n",
                workspace.path().display().to_string()
            ),
            "exec-render-warning",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.member_reconcile_fault("exec/task", None).unwrap(),
            None
        );
        assert!(
            runtime
                .starts
                .lock()
                .unwrap()
                .contains(&"exec.task".to_owned())
        );
        let applied = store
            .observations_for("exec/task", "render.applied")
            .unwrap();
        assert!(
            applied.iter().any(|claim| claim.body["fields"]["warnings"]
                .as_array()
                .is_some_and(|warnings| !warnings.is_empty())),
            "{applied:#?}"
        );
    }

    #[test]
    fn an_eval_cleanup_converges_after_a_runtime_start_failure() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

mission "eval/start-failure" state="ready" timeout="1m" {
  goal "Clean an eval runtime after its start fails."
  agent "worker" { workspace "/tmp"; command "true"; restart "never" }
  step "hold" { goal "Remain active until eval cleanup is requested." }
}
"#;
        apply_source(&store, source, "publish-eval-start-failure");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "eval/start-failure".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-eval-start-failure".into(),
            })
            .unwrap();
        let runtime_id = format!("{}.worker", run.id);
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_starts
            .lock()
            .unwrap()
            .insert(runtime_id.clone());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let subject = format!("agent/{}/worker", run.id);
        assert_eq!(
            reconciler
                .mission_declaration_parses
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "an unchanged run should parse its mission declarations once"
        );
        assert_eq!(
            store
                .latest_actual_value(&subject)
                .unwrap()
                .as_ref()
                .and_then(|actual| actual_field(actual, "status"))
                .and_then(Value::as_str),
            Some("absent")
        );

        store
            .set_mission_run_state(&run.id, "running", "cleanup-cancelled", None)
            .unwrap();
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }

        let current = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(current.status, "cancelled");
        assert_eq!(current.phase, "terminal");
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|desired| desired.owner_run.as_deref() != Some(run.subject.as_str()))
        );
        assert_eq!(&*runtime.removes.lock().unwrap(), &[runtime_id]);
    }

    #[test]
    fn a_successor_generation_retires_members_left_in_its_predecessor() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let publish = |source: &str, key: &str| {
            let intent = parse_intent(source, "node").unwrap();
            let planned = store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            store.apply(&intent, &planned.subject_tokens, key).unwrap();
            intent
                .missions
                .values()
                .next()
                .expect("the fixture has one mission")
                .clone()
        };
        let first = publish(
            r#"
version 2

  mission "retire" state="ready" {
    goal "Retire superseded generation members."
    step "team" {
      goal "Use the first team."

        agent "worker" { workspace "/tmp"; command "true"; restart "never" }

    }
  }

"#,
            "retire-first",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: first.id,
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "retire-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let worker = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == format!("agent/{}/worker", run.id))
            .expect("the first generation member is materialized");
        assert_eq!(worker.owner_run.as_deref(), Some(run.subject.as_str()));
        assert_eq!(
            worker.owner_generation.as_deref(),
            Some(run.generation.as_str())
        );

        let child_mission = publish(
            r#"
version 2

  mission "child" state="ready" {
    goal "Keep one child run active."
    step "work" { goal "Wait for child work." }
  }

"#,
            "retire-child",
        );
        let child = store
            .create_child_mission_run(
                &crate::model::MissionRunRequest {
                    mission: child_mission.id,
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "retire-child-run".into(),
                },
                &run,
                &run.steps[0].subject,
                None,
            )
            .unwrap();
        let grandchild_mission = publish(
            r#"
version 2

  mission "grandchild" state="ready" {
    goal "Keep one grandchild run active."
    step "work" { goal "Wait for grandchild work." }
  }

"#,
            "retire-grandchild",
        );
        let grandchild = store
            .create_child_mission_run(
                &crate::model::MissionRunRequest {
                    mission: grandchild_mission.id,
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "retire-grandchild-run".into(),
                },
                &child,
                &child.steps[0].subject,
                None,
            )
            .unwrap();

        let second = publish(
            r#"
version 2

  mission "retire" state="ready" {
    goal "Retire superseded generation members."
    step "team" { goal "Use the replacement team." }
  }

"#,
            "retire-second",
        );
        let revised = store
            .adopt_mission_revision(
                &run.id,
                &second,
                "person/test",
                "the old team is no longer part of the mission",
                "retire-cutover",
            )
            .unwrap();
        reconciler.reconcile_once().unwrap();

        let desired = store.desired_subjects().unwrap();
        let stop = desired
            .iter()
            .find(|subject| subject.subject == format!("agent/{}/worker", run.id))
            .expect("the predecessor runtime has a teardown declaration");
        assert_eq!(stop.kind, "stop");
        assert_eq!(stop.owner_run.as_deref(), Some(revised.subject.as_str()));
        assert_eq!(
            store.mission_run(&child.id).unwrap().unwrap().status,
            "cancelled"
        );
        assert_eq!(
            store.mission_run(&grandchild.id).unwrap().unwrap().status,
            "cancelled"
        );
        assert_ne!(revised.generation, run.generation);
    }

    #[test]
    fn a_worker_report_waits_for_the_declared_product() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  agent "worker" { workspace "/tmp"; command "true"; restart "never" }
  mission "product" state="ready" {
    goal "Complete mission product."
    completion { when "all-steps-exhausted" }
    step "publish" {
      assigned-to "agent/worker"
        produces {
          resource "mission-run/${ST_MISSION_RUN}/change" {
            kind "custom.st3.product-test"
            state "published"
            recipient "agent/${ST_MISSION_RUN}/worker"
          }
        }
    }
  }

"#;
        apply_source(&store, source, "publish-product");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "product".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-product".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let step = store.mission_run(&run.id).unwrap().unwrap().steps.remove(0);
        assert_eq!(step.status, "ready");
        for action in ["claim", "complete"] {
            store
                .work_action(
                    &step.subject,
                    action,
                    &crate::model::WorkRequest {
                        actor: Some("agent/node.worker".into()),
                        incarnation: Some("test".into()),
                        summary: Some("published the change".into()),
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: format!("{action}-product"),
                    },
                )
                .unwrap();
        }
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "verifying"
        );
        // The idle worker is told once which exact product subject the step waits for.
        reconciler.reconcile_once().unwrap();
        let product_waits = store
            .messages(Some("agent/node.worker"), false)
            .unwrap()
            .into_iter()
            .filter(|message| message.title.as_deref() == Some("Declared product missing: publish"))
            .collect::<Vec<_>>();
        assert_eq!(product_waits.len(), 1, "{product_waits:?}");
        assert!(
            product_waits[0]
                .content
                .contains(&format!("`resource/mission-run/{}/change`", run.id)),
            "{}",
            product_waits[0].content
        );
        assert!(
            product_waits[0]
                .content
                .contains("kind=custom.st3.product-test")
        );
        store
            .append_claim(&ClaimInput {
                subject: format!("resource/mission-run/{}/change", run.id),
                kind: "resource.observed".into(),
                actor: Some("agent/worker".into()),
                fields: BTreeMap::from([
                    (
                        "kind".into(),
                        Value::String("custom.st3.product-test".into()),
                    ),
                    ("state".into(), Value::String("published".into())),
                    (
                        "recipient".into(),
                        Value::String(format!("agent/{}/worker", run.id)),
                    ),
                    ("message".into(), Value::String("extra-field".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("product-binding".into()),
            })
            .unwrap();
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn a_step_uses_the_exact_mission_revision_produced_by_an_earlier_step() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let bootstrap = r#"
version 2

  agent "planner" { workspace "/tmp"; command "true"; restart "never" }
  mission "bootstrap" state="ready" {
    goal "Complete mission bootstrap."
    completion { when "all-steps-exhausted" }
    step "compile" {
      assigned-to "agent/planner"
      produces-mission "project/work"
    }
    step "execute" {
      assigned-to "agent/planner"
      depends-on { step "compile" completed }
      uses-mission output-of="compile"
    }
  }

"#;
        let first_work = r#"
version 2

  mission "project/work" state="ready" {
    goal "Complete mission project/work."
    completion { when "all-steps-exhausted" }
    step "inspect" { title "Inspect the fixture" }
    step "finish" { depends-on { step "inspect" completed } }
  }

"#;
        apply_source(&store, bootstrap, "publish-bootstrap");
        apply_source(&store, first_work, "publish-first-work");
        let first = store
            .mission_spec("project/work", None)
            .unwrap()
            .expect("first work mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "bootstrap".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-bootstrap".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let compile = store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .into_iter()
            .find(|step| step.step == "compile")
            .unwrap();
        store
            .work_action(
                &compile.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.planner".into()),
                    incarnation: Some("test".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-compile".into(),
                },
            )
            .unwrap();
        let output = store
            .record_mission_output(
                &compile.subject,
                "agent/node.planner",
                Some("test"),
                "project/work",
                &first,
                "bind-first-work",
            )
            .unwrap();
        store
            .work_action(
                &compile.subject,
                "complete",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.planner".into()),
                    incarnation: Some("test".into()),
                    summary: Some("published the complete mission".into()),
                    reason: None,
                    evidence: vec![output.claim_id.clone()],
                    idempotency_key: "complete-compile".into(),
                },
            )
            .unwrap();

        let second_work = r#"
version 2

  mission "project/work" state="ready" {
    goal "Complete mission project/work."
    step "replacement" { title "A later mission revision" }
  }

"#;
        apply_source(&store, second_work, "publish-second-work");
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        let execute = store
            .mission_run(&run.id)
            .unwrap()
            .unwrap()
            .steps
            .into_iter()
            .find(|step| step.step == "execute")
            .unwrap();
        assert_eq!(execute.status, "ready");
        for action in ["claim", "complete"] {
            store
                .work_action(
                    &execute.subject,
                    action,
                    &crate::model::WorkRequest {
                        actor: Some("agent/node.planner".into()),
                        incarnation: Some("test".into()),
                        summary: None,
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: format!("{action}-execute"),
                    },
                )
                .unwrap();
        }
        reconciler.reconcile_once().unwrap();
        let child = store
            .mission_run_for_parent_step(&execute.subject)
            .unwrap()
            .expect("the used mission run");
        assert_eq!(child.revision, first.revision);
        assert_eq!(child.root_mission_run, run.subject);
        assert_eq!(
            child.parent_step_run.as_deref(),
            Some(execute.subject.as_str())
        );
        assert!(
            child
                .steps
                .iter()
                .all(|step| step.assigned_to.as_deref() == Some("agent/node.planner"))
        );
        assert_eq!(
            child
                .steps
                .iter()
                .map(|step| step.step.as_str())
                .collect::<Vec<_>>(),
            vec!["finish", "inspect"]
        );

        for step_name in ["inspect", "finish"] {
            for _ in 0..3 {
                reconciler.reconcile_once().unwrap();
            }
            let step = store
                .mission_run(&child.id)
                .unwrap()
                .unwrap()
                .steps
                .into_iter()
                .find(|step| step.step == step_name)
                .unwrap();
            assert_eq!(step.status, "ready");
            for action in ["claim", "complete"] {
                store
                    .work_action(
                        &step.subject,
                        action,
                        &crate::model::WorkRequest {
                            actor: Some("agent/node.planner".into()),
                            incarnation: Some("test".into()),
                            summary: None,
                            reason: None,
                            evidence: Vec::new(),
                            idempotency_key: format!("{action}-{step_name}"),
                        },
                    )
                    .unwrap();
            }
        }
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        let completed = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(
            store.mission_run(&child.id).unwrap().unwrap().status,
            "completed"
        );
        let replica = Store::open_memory("replica").unwrap();
        replica
            .import_replication("node", &store.export_replication(0).unwrap())
            .unwrap();
        let replicated_child = replica
            .mission_run(&child.id)
            .unwrap()
            .expect("the replicated child mission run");
        assert_eq!(replicated_child.root_mission_run, run.subject);
        assert_eq!(replicated_child.parent_step_run, child.parent_step_run);
        assert_eq!(replicated_child.revision, first.revision);
        assert_eq!(
            replicated_child
                .steps
                .iter()
                .map(|step| step.assigned_to.as_deref())
                .collect::<Vec<_>>(),
            child
                .steps
                .iter()
                .map(|step| step.assigned_to.as_deref())
                .collect::<Vec<_>>()
        );
        assert!(
            replica
                .mission_output(&compile.subject, 1, &compile.definition_hash)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn an_assignment_waits_until_the_agent_is_in_the_desired_graph() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2

  mission "assignment" state="ready" {
    goal "Complete mission assignment."
    step "work" { assigned-to "agent/worker" }
  }

"#,
            "assignment-mission",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "assignment".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "assignment-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let blocked = store.mission_run(&run.id).unwrap().unwrap().steps.remove(0);
        assert_eq!(blocked.status, "blocked");
        assert!(
            blocked
                .blocked_reason
                .unwrap()
                .contains("no eligible agent is present in the desired graph")
        );

        apply_source(
            &store,
            r#"version 2
 stop "agent/node.worker" "#,
            "assignment-stopped-agent",
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "blocked"
        );

        apply_source(
            &store,
            r#"version 2
 agent "worker" { workspace "/tmp"; command "true"; restart "never" } "#,
            "assignment-agent",
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "ready"
        );
    }

    #[test]
    fn a_step_can_materialize_its_own_assigned_agent() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2

mission "self-assigned" state="ready" {
  goal "Bring up the judge before offering its work."
  step "judge" {
    assigned-to "agent/${ST_MISSION_RUN}/judge"
    agent "judge" { workspace "/tmp"; command "true"; restart "never" }
  }
}
"#,
            "self-assigned-mission",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "self-assigned".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "self-assigned-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let judge = store.mission_run(&run.id).unwrap().unwrap().steps.remove(0);
        assert_eq!(judge.status, "ready");
        assert_eq!(
            judge.assigned_to.as_deref(),
            Some(format!("agent/{}/judge", run.id).as_str())
        );
        assert!(store.desired_subjects().unwrap().iter().any(|desired| {
            desired.subject == format!("agent/{}/judge", run.id)
                && desired.owner_step.as_deref() == Some(judge.subject.as_str())
        }));
    }

    #[test]
    fn a_human_gate_accepts_only_the_bound_step_run_review() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2

  mission "review" state="ready" {
    goal "Complete mission review."
    step "approval" {
      title "The candidate change"
      gate "human-review" type="human" {
        reviewer "person/nathan"
        question "Is the candidate ready?"
        review "resource/mission-run/${ST_MISSION_RUN}/candidate"
      }
    }
  }

"#,
            "review-mission",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "review".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "review-run".into(),
            })
            .unwrap();
        let step = run.steps[0].subject.clone();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let request = store
            .gate_request_for_owner(&step)
            .unwrap()
            .expect("the human review was not requested");
        assert_eq!(
            request
                .body
                .pointer("/fields/question")
                .and_then(Value::as_str),
            Some("Is the candidate ready?")
        );
        assert_eq!(
            request
                .body
                .pointer("/fields/review_targets/0")
                .and_then(Value::as_str),
            Some(format!("resource/mission-run/{}/candidate", run.id).as_str())
        );
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/someone-else".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    ("request".into(), Value::String(request.id.clone())),
                ]),
                evidence: vec![request.id.clone()],
                expected_subject: None,
                idempotency_key: Some("wrong-reviewer".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "working",
            "a wrong reviewer cannot complete the active review"
        );
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([("verdict".into(), Value::String("pass".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("unbound-review".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "working",
            "an unbound verdict cannot complete the active review"
        );
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    ("request".into(), Value::String(request.id.clone())),
                ]),
                evidence: vec![request.id],
                expected_subject: None,
                idempotency_key: Some("right-reviewer".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "completed"
        );
        store
            .set_mission_run_state(&run.id, "completed", "terminal", None)
            .unwrap();

        let rejected_run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "review".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "review-rejected-run".into(),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let rejected_step = &rejected_run.steps[0].subject;
        let rejected_request = store
            .gate_request_for_owner(rejected_step)
            .unwrap()
            .expect("the second review was not requested");
        store
            .append_claim(&ClaimInput {
                subject: rejected_request.subject,
                kind: "gate.result".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("fail".into())),
                    (
                        "reason".into(),
                        Value::String("The proof needs a source.".into()),
                    ),
                    ("request".into(), Value::String(rejected_request.id)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("review-rejected-result".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        let failed = store.step_run(rejected_step).unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert!(
            failed
                .blocked_reason
                .as_deref()
                .unwrap_or_default()
                .contains("The proof needs a source.")
        );
    }

    #[test]
    fn feedback_review_retries_with_written_goals_and_messages_the_worker() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
agent "worker" { workspace "/tmp"; command "true" }
mission "feedback-review" state="ready" {
  goal "Review a draft."
  step "draft" {
    assigned-to "agent/worker"
    goal "Submit a draft."
    gate "review" type="human" mode="feedback" { reviewer "person/operator" }
  }
}
"#,
            "feedback-review-source",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "feedback-review".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "feedback-review-run".into(),
            })
            .unwrap();
        let step = &run.steps[0];
        let claimant = step.assigned_to.clone().unwrap();
        store.set_step_state(&step.subject, "ready", None).unwrap();
        let work = |key: &str| crate::model::WorkRequest {
            actor: Some(claimant.clone()),
            incarnation: Some("test-incarnation".into()),
            summary: Some("Draft submitted".into()),
            reason: None,
            evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        store
            .work_action(&step.subject, "claim", &work("draft-claim-1"))
            .unwrap();
        store
            .work_action(&step.subject, "complete", &work("draft-submit-1"))
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let request = store
            .gate_request_for_owner(&step.subject)
            .unwrap()
            .unwrap();
        assert_eq!(
            request.body.pointer("/fields/mode").and_then(Value::as_str),
            Some("feedback")
        );
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/operator".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("feedback".into())),
                    (
                        "reason".into(),
                        Value::String("Add a source for the estimate.".into()),
                    ),
                    ("request".into(), Value::String(request.id.clone())),
                ]),
                evidence: vec![request.id],
                expected_subject: None,
                idempotency_key: Some("draft-feedback-1".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        let retried = store.step_run(&step.subject).unwrap().unwrap();
        assert_eq!(retried.attempt, 2);
        assert_ne!(retried.status, "failed");
        assert!(
            retried
                .goals
                .iter()
                .any(|goal| goal.contains("Add a source for the estimate."))
        );
        let retry_claim = store
            .latest_claim(&step.subject, Some("step-run.retried"))
            .unwrap()
            .unwrap();
        assert!(
            retry_claim
                .body
                .pointer("/fields/not_before_unix_ms")
                .unwrap()
                .is_null()
        );
        let messages = store.messages(Some(&claimant), false).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .content
                .contains("Add a source for the estimate.")
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(store.messages(Some(&claimant), false).unwrap().len(), 1);
    }

    #[test]
    fn rejects_an_unstructured_usage_log() {
        assert_eq!(structured_token_usage("the gate finished"), None);
    }

    #[test]
    fn a_started_member_gets_its_graph_identity() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            r#"
            version 2

              agent "worker" {{
                workspace {:?}
                command "true"
              }}

        "#,
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "member-identity");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        let members = runtime.started_members.lock().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(
            members[0].environment.get("ST_AGENT").map(String::as_str),
            Some("agent/node.worker")
        );
        assert_eq!(
            members[0].environment.get("ST3_BIN").map(PathBuf::from),
            Some(std::env::current_exe().unwrap())
        );
        assert!(!members[0].environment.contains_key("PATH"));
    }

    #[test]
    fn a_native_driver_uses_the_running_st3_executable() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            r#"
            version 2

              agent "worker" {{
                workspace {:?}
                harness "codex" {{ prompt "Wait for work." }}
              }}

        "#,
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "native-driver-executable");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        let members = runtime.started_members.lock().unwrap();
        let LaunchSpec::Argv(argv) = &members[0].launch else {
            panic!("the native driver launch is not argv");
        };
        assert_eq!(
            argv.first().map(Path::new),
            Some(std::env::current_exe().unwrap().as_path())
        );
    }

    #[test]
    fn file_gate_watchers_are_released_without_active_gates() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("gate.txt");
        std::fs::write(&path, "ready").unwrap();
        let subject = format!("file/node:{}", path.display());
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.ensure_file_observation(&subject).unwrap();
        assert_eq!(reconciler.file_watchers.lock().unwrap().len(), 1);
        std::fs::write(&path, "updated and ready").unwrap();
        reconciler.ensure_file_observation(&subject).unwrap();
        assert_eq!(
            reconciler
                .store
                .latest_actual_value(&subject)
                .unwrap()
                .unwrap()["content"],
            "updated and ready"
        );
        reconciler.reconcile_once().unwrap();
        assert!(reconciler.file_watchers.lock().unwrap().is_empty());
    }

    #[test]
    fn one_uncreatable_workspace_does_not_starve_other_agent_launches() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "not a directory").unwrap();
        let source = format!(
            "version 2\nagent \"bad\" {{ workspace {:?} create=#true; harness \"codex\" {{}} }}\nagent \"good\" {{ workspace {:?}; harness \"codex\" {{}} }}\n",
            root.path().join("file/child").display().to_string(),
            root.path().display().to_string(),
        );
        apply_source(&store, &source, "unavailable-workspace");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        let started = runtime.started_members.lock().unwrap();
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].runtime_id, "node.good");
        let diagnostic = store
            .claims_for("agent/node.bad", Some("runtime.reconcile-decision"))
            .unwrap();
        assert_eq!(diagnostic.len(), 1);
        assert_eq!(
            diagnostic[0]
                .body
                .pointer("/fields/decision")
                .and_then(Value::as_str),
            Some("member-fault")
        );
    }

    #[test]
    fn a_boot_render_refusal_prevents_the_agent_start() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        std::fs::create_dir_all(workspace.path().join(".st3")).unwrap();
        std::fs::write(
            workspace.path().join(".st3/boot.md"),
            "repository-owned boot text\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", ".st3/boot.md"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{}} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "boot-render-refusal");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        assert!(
            store
                .member_reconcile_fault("agent/node.worker", None)
                .unwrap()
                .unwrap()
                .contains("tracked file")
        );
        assert!(runtime.starts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_failed_render_isolated_from_healthy_members_and_clears_on_recovery() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        fs::create_dir(workspace.path().join(".claude")).unwrap();
        fs::write(workspace.path().join(".claude/settings.local.json"), "{}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", ".claude/settings.local.json"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        let source = format!(
            r#"version 2
agent "bad" {{ workspace {:?}; command "true"; render {{ file ".claude/settings.local.json" "changed" }} }}
agent "good" {{ workspace {:?}; command "true"; render {{ file "healthy" "rendered" }} }}
"#,
            workspace.path().display().to_string(),
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "render-isolation");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(&*runtime.starts.lock().unwrap(), &["node.good"]);
        assert_eq!(
            fs::read_to_string(workspace.path().join("healthy")).unwrap(),
            "rendered"
        );
        let reason = store
            .member_reconcile_fault("agent/node.bad", None)
            .unwrap()
            .unwrap();
        assert!(reason.contains("tracked file") && reason.contains("settings.local.json"));
        assert!(
            store
                .member_reconcile_fault("agent/node.good", None)
                .unwrap()
                .is_none()
        );
        let failed_at = store.index().unwrap();
        // Removing the bad render also clears its fault, even with no remaining render writes.
        let fixed = format!(
            r#"version 2
agent "bad" {{ workspace {:?}; command "true" }}
"#,
            workspace.path().display().to_string()
        );
        apply_source(&store, &fixed, "render-recovered");
        reconciler.reconcile_once().unwrap();
        assert!(
            runtime
                .starts
                .lock()
                .unwrap()
                .contains(&"node.bad".to_owned())
        );
        assert!(
            store
                .member_reconcile_fault("agent/node.bad", None)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .member_reconcile_fault("agent/node.bad", Some(failed_at))
                .unwrap()
                .is_some()
        );
    }

    /// Tonight's seats: a running seat whose tracked settings file no longer matches its render.
    #[test]
    fn a_running_seat_whose_render_fails_is_still_watched_and_woken_but_never_restarted() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        fs::create_dir(workspace.path().join(".claude")).unwrap();
        fs::write(workspace.path().join(".claude/settings.local.json"), "{}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", ".claude/settings.local.json"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        let workspace_path = workspace.path().display().to_string();
        apply_source(
            &store,
            &format!(
                r#"version 2
agent "seat" {{ workspace {workspace_path:?}; command "true"; render {{ file ".claude/settings.local.json" "changed" }} }}
mission "task" state="ready" {{
  goal "Give the seat work."
  step "work" {{ assigned-to "agent/node.seat"; goal "Do the work." }}
}}
"#
            ),
            "running-render-failure",
        );
        store
            .create_mission_run(&MissionRunRequest {
                mission: "task".into(),
                revision: None,
                workspace: workspace_path.clone(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "task-run".into(),
            })
            .unwrap();
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == "agent/node.seat")
            .and_then(|desired| desired.member)
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .ptys
            .lock()
            .unwrap()
            .push(running_observation(&member, "one"));
        mark_harness_ready(&store, "agent/node.seat", &member, "pi", "one");
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let reason = store
            .member_reconcile_fault("agent/node.seat", None)
            .unwrap()
            .expect("the render failure is recorded on the seat");
        assert!(reason.contains("tracked file"), "{reason}");
        assert!(
            !store
                .work_wake_messages_for_reconcile("agent/node.seat")
                .unwrap()
                .is_empty(),
            "the running seat was not woken for its ready work"
        );
        let decisions = store
            .claims_for("agent/node.seat", Some("runtime.reconcile-decision"))
            .unwrap()
            .into_iter()
            .filter(|claim| claim.body["fields"]["key"] == "member-reconcile")
            .map(|claim| {
                claim.body["fields"]["decision"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decisions,
            ["member-fault"],
            "one fault, never a false recovery"
        );

        // The seat exits. Its render still fails, so it is observed but not started again.
        runtime.ptys.lock().unwrap()[0].status = "exited".into();
        reconciler.reconcile_once().unwrap();
        assert!(runtime.starts.lock().unwrap().is_empty());
        assert_eq!(
            store
                .latest_actual_value("agent/node.seat")
                .unwrap()
                .and_then(|actual| actual_field(&actual, "status").cloned()),
            Some(Value::String("exited".into()))
        );
    }

    /// Declaring a member that would render different bytes to a file another member already
    /// rendered faults only the newcomer. The member that owns the file keeps running.
    #[test]
    fn a_render_conflict_faults_only_the_member_that_would_change_the_file() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        let path = workspace.path().display().to_string();
        apply_source(
            &store,
            &format!(
                "version 2\nagent \"first\" {{ workspace {path:?}; command \"true\"; render {{ file \"shared\" \"one\" }} }}\n"
            ),
            "render-first",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("shared")).unwrap(),
            "one"
        );
        apply_source(
            &store,
            &format!(
                "version 2\nagent \"second\" {{ workspace {path:?}; command \"true\"; render {{ file \"shared\" \"two\" }} }}\n"
            ),
            "render-second",
        );
        reconciler.reconcile_once().unwrap();
        assert!(
            store
                .member_reconcile_fault("agent/node.first", None)
                .unwrap()
                .is_none(),
            "the member that owns the file was faulted"
        );
        let reason = store
            .member_reconcile_fault("agent/node.second", None)
            .unwrap()
            .expect("the newcomer records the conflict");
        assert!(reason.contains("disagree"), "{reason}");
        assert_eq!(
            fs::read_to_string(workspace.path().join("shared")).unwrap(),
            "one"
        );
        assert!(
            !runtime
                .starts
                .lock()
                .unwrap()
                .contains(&"node.second".to_owned())
        );
    }

    #[test]
    fn a_failed_start_isolated_from_healthy_members() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "bad" { workspace "/tmp"; command "true" }
agent "good" { workspace "/tmp"; command "true" }
"#,
            "start-isolation",
        );
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_starts
            .lock()
            .unwrap()
            .insert("node.bad".into());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            runtime.started_members.lock().unwrap()[0].runtime_id,
            "node.good"
        );
        assert!(
            store
                .member_reconcile_fault("agent/node.bad", None)
                .unwrap()
                .unwrap()
                .contains("fake runtime rejected")
        );
        assert!(
            store
                .member_reconcile_fault("agent/node.good", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_failed_observation_does_not_starve_a_healthy_member() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
exec "bad" { workspace "/tmp"; command "true" }
exec "good" { workspace "/tmp"; command "true" }
"#,
            "observe-isolation",
        );
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_observes
            .lock()
            .unwrap()
            .insert("exec.bad".into());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(&*runtime.starts.lock().unwrap(), &["exec.good"]);
        assert!(
            store
                .member_reconcile_fault("exec/bad", None)
                .unwrap()
                .unwrap()
                .contains("fake observe failed")
        );
    }

    #[test]
    fn a_failed_stop_does_not_starve_a_healthy_member() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "bad" { workspace "/tmp"; command "true" }
"#,
            "stop-isolation-start",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.bad".into(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("bad-one".into()),
        });
        runtime
            .failed_stops
            .lock()
            .unwrap()
            .insert("node.bad".into());
        apply_source(
            &store,
            r#"version 2
stop "agent/node.bad"
agent "good" { workspace "/tmp"; command "true" }
"#,
            "stop-isolation-stop",
        );
        reconciler.reconcile_once().unwrap();
        assert!(
            runtime
                .starts
                .lock()
                .unwrap()
                .contains(&"node.good".to_owned())
        );
        assert!(
            store
                .member_reconcile_fault("agent/node.bad", None)
                .unwrap()
                .unwrap()
                .contains("fake stop failed")
        );
    }

    #[test]
    fn a_superseded_member_stops_even_when_its_render_now_fails() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let first = format!(
            r#"version 2
mission "render-retire" state="ready" {{
 goal "Retire a broken renderer."
 agent "worker" {{ workspace {:?}; command "true"; render {{ file "output" "ready" }} }}
 step "wait" {{ goal "Wait for replacement."; gate "hold" {{ field "state" "resource/not-ready" is "ready" }} }}
}}
"#,
            workspace.path().display().to_string()
        );
        apply_source(&store, &first, "render-retire-first");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "render-retire".into(),
                revision: None,
                workspace: workspace.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "render-retire-run".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|s| s.subject == format!("agent/{}/worker", run.id))
            .unwrap();
        let runtime_id = member.member.as_ref().unwrap().runtime_id.clone();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("retired-one".into()),
        });
        fs::remove_file(workspace.path().join("output")).unwrap();
        fs::create_dir(workspace.path().join("output")).unwrap();
        assert!(crate::render::apply(&store, &member.desired, workspace.path()).is_err());
        let second = r#"version 2
mission "render-retire" state="ready" { goal "Retire a broken renderer."; step "wait" { goal "Replacement without the worker." } }
"#;
        apply_source(&store, second, "render-retire-second");
        let intent = parse_intent(second, "node").unwrap();
        store
            .adopt_mission_revision(
                &run.id,
                intent.missions.values().next().unwrap(),
                "person/test",
                "replace broken renderer",
                "render-retire-adopt",
            )
            .unwrap();
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        assert!(runtime.stops.lock().unwrap().contains(&runtime_id));
        assert!(workspace.path().join("output").is_dir());
        assert!(
            store
                .member_reconcile_fault(&member.subject, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_mission_step_waits_for_native_driver_readiness_before_starting_a_gate() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = r#"
            version 2

              mission "proof" state="ready" {
                goal "Complete mission proof."
                completion { when "all-steps-exhausted" }
                step "native-ready" {
                  title "The native agent is ready"

                    agent "worker" {
                      harness "codex" { prompt "Do the work." }
                    }
                    message "kick" {
                      from "requester"
                      to "worker"
                      content "Start."
                    }

                  gate "verify" {
                    exec "true"
                    host "node"
                    workspace "."
                    time-limit "1m"
                  }
                }
              }

        "#;
        apply_source(&store, source, "mission-native-ready");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "proof".into(),
                revision: None,
                workspace: workspace.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-native-ready".into(),
            })
            .unwrap();
        let runtime_id = format!("{}.worker", run.id);
        let agent_subject = format!("agent/{}/worker", run.id);
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            &*runtime.starts.lock().unwrap(),
            std::slice::from_ref(&runtime_id)
        );

        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        });
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            &*runtime.starts.lock().unwrap(),
            std::slice::from_ref(&runtime_id)
        );

        store
            .append_claim(&ClaimInput {
                subject: agent_subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(agent_subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("transport".into(), Value::String("app-server".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-ready".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(&*runtime.starts.lock().unwrap(), &[runtime_id]);

        store
            .append_claim(&ClaimInput {
                subject: "message/kick".into(),
                kind: "message.delivered".into(),
                actor: Some(agent_subject),
                fields: BTreeMap::from([("status".into(), Value::String("delivered".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("kick-delivered".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();

        let starts = runtime.starts.lock().unwrap();
        assert_eq!(starts.len(), 2);
        assert!(starts[1].starts_with("gate-operation."));
    }

    #[test]
    fn a_mission_step_fails_when_its_non_restarting_driver_exits_before_readiness() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = r#"
            version 2

              mission "driver-start-failure" state="ready" {
                goal "Expose a driver that cannot become ready."
                completion { when "all-steps-exhausted" }
                step "start-agent" timeout="10m" {
                  title "The native agent is ready"
                  agent "worker" {
                    harness "codex" { prompt "Do the work." }
                    restart "never"
                  }
                }
              }

        "#;
        apply_source(&store, source, "driver-start-failure");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "driver-start-failure".into(),
                revision: None,
                workspace: workspace.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-driver-start-failure".into(),
            })
            .unwrap();
        let runtime_id = format!("{}.worker", run.id);
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id,
            terminal: true,
            status: "exited".into(),
            exit_code: Some(2),
            incarnation_id: Some("failed-start".into()),
        });
        reconciler.reconcile_once().unwrap();

        let run = store.mission_run(&run.id).unwrap().unwrap();
        let step = run
            .steps
            .iter()
            .find(|step| step.step == "start-agent")
            .unwrap();
        assert_eq!(step.status, "failed");
        assert!(
            step.blocked_reason
                .as_deref()
                .unwrap()
                .contains("could not become ready: runtime status was exited with exit code 2")
        );
    }

    #[test]
    fn a_simulated_codex_graph_reaches_completion_and_cleans_its_runtimes() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = r#"
            version 2

              mission "eval/simulated-codex" state="ready" timeout="5m" {
                goal "Complete mission eval/simulated-codex."
                step "team" {
                  title "The Codex team is ready"

                    agent "sup" {
                      harness "codex" { prompt "Coordinate the work." }
                      restart "never"
                    }
                    agent "worker" {
                      harness "codex" { prompt "Do the work." }
                      restart "never"
                    }
                    message "kickoff" {
                      from "requester"
                      to "sup"
                      content "Start."
                    }

                  gate "condition-1" { exists "agent/${ST_MISSION_RUN}/sup" }
                  gate "condition-2" { exists "agent/${ST_MISSION_RUN}/worker" }
                }
                step "worker-report" {
                  title "The worker report is delivered"
                  depends-on { step "team" completed }

                    message "worker-report" {
                      from "worker"
                      to "sup"
                      content "The work is complete."
                    }

                }
                step "confirmation" {
                  title "The supervisor confirmation is delivered"
                  depends-on { step "worker-report" completed }

                    message "confirmation" {
                      from "sup"
                      to "requester"
                      content "The result is verified."
                    }

                }
                step "mechanical" {
                  title "The mechanical gate passes"
                  depends-on { step "confirmation" completed }
                  gate "mechanical" {
                    exec "true"
                    host "node"
                    workspace "."
                    time-limit "60s"
                  }
                }
                step "semantic" {
                  title "The Codex gate passes"
                  depends-on { step "mechanical" completed }
                  gate "semantic" type="llm" {
                    model "gpt-5.6-sol"
                    host "node"
                    workspace "."
                    tools "shell"
                    token-budget 1000
                    time-limit "60s"
                    prompt "Check the result."
                  }
                }
                completion { when "all-steps-exhausted" }
              }

        "#;
        apply_source(&store, source, "simulated-codex-graph");
        let mission_run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "eval/simulated-codex".into(),
                revision: None,
                workspace: workspace.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-simulated-codex".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let deliver = |subject: &str, recipient: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "message.delivered".into(),
                    actor: Some(recipient.into()),
                    fields: BTreeMap::from([("status".into(), Value::String("delivered".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("deliver:{subject}")),
                })
                .unwrap();
        };

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.started_members.lock().unwrap().len(), 2);
        let sup_runtime = format!("{}.sup", mission_run.id);
        let worker_runtime = format!("{}.worker", mission_run.id);
        let sup_subject = format!("agent/{}/sup", mission_run.id);
        let worker_subject = format!("agent/{}/worker", mission_run.id);
        runtime.ptys.lock().unwrap().extend([
            RuntimeObservation {
                runtime_id: sup_runtime,
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("sup-one".into()),
            },
            RuntimeObservation {
                runtime_id: worker_runtime,
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("worker-one".into()),
            },
        ]);
        reconciler.reconcile_once().unwrap();
        for subject in [&sup_subject, &worker_subject] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: "harness.observed".into(),
                    actor: Some(subject.clone()),
                    fields: BTreeMap::from([
                        ("state".into(), Value::String("ready".into())),
                        ("transport".into(), Value::String("app-server".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("ready:{subject}")),
                })
                .unwrap();
        }
        deliver("message/kickoff", &sup_subject);
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        deliver("message/worker-report", &sup_subject);
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        deliver("message/confirmation", "requester");
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }

        let mechanical = runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .find(|member| member.driver.as_deref() == Some("mechanical-gate"))
            .unwrap()
            .runtime_id
            .clone();
        runtime.execs.lock().unwrap().insert(
            mechanical.clone(),
            RuntimeObservation {
                runtime_id: mechanical,
                terminal: false,
                status: "exited".into(),
                exit_code: Some(0),
                incarnation_id: Some("mechanical-one".into()),
            },
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }

        let (llm, gate_subject) = {
            let members = runtime.started_members.lock().unwrap();
            let member = members
                .iter()
                .find(|member| member.driver.as_deref() == Some("llm-gate"))
                .unwrap();
            (
                member.runtime_id.clone(),
                member.environment["ST_GATE_SUBJECT"].clone(),
            )
        };
        store
            .append_claim(&ClaimInput {
                subject: gate_subject,
                kind: "gate.result".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    (
                        "reason".into(),
                        Value::String("the result is correct".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("simulated-llm-result".into()),
            })
            .unwrap();
        runtime.execs.lock().unwrap().insert(
            llm.clone(),
            RuntimeObservation {
                runtime_id: llm.clone(),
                terminal: false,
                status: "exited".into(),
                exit_code: Some(0),
                incarnation_id: Some("llm-one".into()),
            },
        );
        runtime.logs.lock().unwrap().insert(
            llm,
            r#"{"type":"turn.completed","usage":{"total_tokens":120}}"#.into(),
        );
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.stops.lock().unwrap().len(), 2);

        runtime.ptys.lock().unwrap().clear();
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }

        let completed = store.mission_run(&mission_run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed", "{completed:?}");
        assert!(
            store.desired_subjects().unwrap().iter().all(|desired| {
                desired.owner_run.as_deref() != Some(mission_run.subject.as_str())
            })
        );
        assert_eq!(
            store
                .latest_claim(&mission_run.subject, Some("eval.verdict"))
                .unwrap()
                .unwrap()
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("pass")
        );
        let started = runtime.starts.lock().unwrap().clone();
        let removed = runtime.removes.lock().unwrap().clone();
        assert!(
            started
                .iter()
                .all(|runtime_id| removed.contains(runtime_id)),
            "eval cleanup must remove each runtime record; started={:?}; removed={:?}",
            started,
            removed,
        );
    }

    #[test]
    fn a_mechanical_gate_uses_the_async_exec_runtime() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "proof" state="ready" {
                goal "Complete mission proof."
                completion { when "all-steps-exhausted" }
                step "verify" {
                  title "The command passes"
                  gate "verify" {
                    exec "sleep 60"
                    host "node"
                    workspace "."
                    time-limit "1m"
                  }
                }
              }

        "#;
        apply_source(&store, source, "mission-async-gate");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "proof".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-async-gate".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        let runtime_id = runtime.starts.lock().unwrap()[0].clone();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "running"
        );
        runtime.execs.lock().unwrap().insert(
            runtime_id.clone(),
            RuntimeObservation {
                runtime_id,
                terminal: false,
                status: "exited".into(),
                exit_code: Some(0),
                incarnation_id: Some("gate-one".into()),
            },
        );

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }

        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_mechanical_gate_that_outlives_a_daemon_restart_takes_its_drivers_exit_status() {
        // After a restart the runner is no longer the daemon's child: the exec runtime finds it
        // gone without an exit status. Only its `st3 driver exec` report says how it ended.
        for (report, run_status, verdict) in [
            (Some(0), "completed", "pass"),
            (Some(1), "failed", "fail"),
            (None, "failed", "fail"),
        ] {
            let store = Arc::new(Store::open_memory("node").unwrap());
            let source = r#"
                version 2

                  mission "proof" state="ready" {
                    goal "Complete mission proof."
                    completion { when "all-steps-exhausted" }
                    step "verify" {
                      title "The command passes"
                      gate "verify" {
                        exec "sleep 60"
                        host "node"
                        workspace "."
                        time-limit "1m"
                      }
                    }
                  }

            "#;
            apply_source(&store, source, "mission-restarted-gate");
            let run = store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: "proof".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "run-restarted-gate".into(),
                })
                .unwrap();
            let runtime = Arc::new(FakeRuntime::default());
            let reconciler = Reconciler::new(
                store.clone(),
                runtime.clone(),
                "node".into(),
                Arc::new(Notify::new()),
            );
            for _ in 0..4 {
                reconciler.reconcile_once().unwrap();
            }
            let gate = runtime
                .started_members
                .lock()
                .unwrap()
                .iter()
                .find(|member| member.driver.as_deref() == Some("mechanical-gate"))
                .cloned()
                .expect("the mechanical gate did not start");
            let crate::model::LaunchSpec::Argv(argv) = &gate.launch else {
                panic!("the gate runner is not wrapped by its driver");
            };
            let subject = argv[argv.iter().position(|arg| arg == "--subject").unwrap() + 1].clone();
            if let Some(code) = report {
                store
                    .append_claim(&ClaimInput {
                        subject: subject.clone(),
                        kind: "runtime.observed".into(),
                        actor: Some(subject.clone()),
                        fields: BTreeMap::from([
                            ("status".into(), Value::String("exited".into())),
                            ("exit_code".into(), Value::from(code)),
                            ("exit_signal".into(), Value::Null),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
            }
            runtime.execs.lock().unwrap().insert(
                gate.runtime_id.clone(),
                RuntimeObservation {
                    runtime_id: gate.runtime_id,
                    terminal: false,
                    status: "exited".into(),
                    exit_code: None,
                    incarnation_id: Some("gate-one".into()),
                },
            );

            for _ in 0..3 {
                reconciler.reconcile_once().unwrap();
            }

            let result = store
                .latest_claim(&subject, Some("gate.result"))
                .unwrap()
                .expect("the gate has no result");
            assert_eq!(result.body["fields"]["verdict"], verdict, "{report:?}");
            if report.is_none() {
                assert_eq!(
                    result.body["fields"]["reason"],
                    "mechanical gate `verify` fail: it exited without an exit status"
                );
            }
            assert_eq!(
                store.mission_run(&run.id).unwrap().unwrap().status,
                run_status,
                "{report:?}"
            );
        }
    }

    #[test]
    fn an_llm_gate_fails_when_structured_usage_exceeds_its_budget() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "proof" state="ready" {
                goal "Complete mission proof."
                step "review" {
                  title "A held-out gate accepts the result"
                  gate "review" type="llm" {
                    model "claude-sonnet"
                    host "node"
                    workspace "."
                    tools "shell"
                    token-budget 10
                    time-limit "1m"
                    prompt "Inspect the result."
                  }
                }
              }

        "#;
        apply_source(&store, source, "mission-llm-budget");
        store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "proof".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-llm-budget".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let (runtime_id, result_subject) = {
            let members = runtime.started_members.lock().unwrap();
            let gate = members
                .iter()
                .find(|member| member.driver.as_deref() == Some("llm-gate"))
                .unwrap();
            assert_eq!(
                gate.environment.get("ST3_BIN").map(Path::new),
                Some(std::env::current_exe().unwrap().as_path())
            );
            let LaunchSpec::Argv(argv) = &gate.launch else {
                panic!("the LLM gate launch is not argv");
            };
            assert!(
                argv.last()
                    .is_some_and(|prompt| prompt.contains("\"$ST3_BIN\" gate-result pass"))
            );
            (
                gate.runtime_id.clone(),
                gate.environment["ST_GATE_SUBJECT"].clone(),
            )
        };
        store
            .append_claim(&ClaimInput {
                subject: result_subject.clone(),
                kind: "gate.result".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    (
                        "reason".into(),
                        Value::String("the result is correct".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("llm-preliminary-result".into()),
            })
            .unwrap();
        runtime.execs.lock().unwrap().insert(
            runtime_id.clone(),
            RuntimeObservation {
                runtime_id: runtime_id.clone(),
                terminal: false,
                status: "exited".into(),
                exit_code: Some(0),
                incarnation_id: Some("gate-run".into()),
            },
        );
        runtime.logs.lock().unwrap().insert(
            runtime_id,
            r#"{"type":"result","usage":{"input_tokens":8,"output_tokens":4}}"#.into(),
        );

        reconciler.reconcile_once().unwrap();

        let result = store
            .latest_claim(&result_subject, Some("gate.result"))
            .unwrap()
            .unwrap();
        assert_eq!(
            result
                .body
                .pointer("/fields/token_usage")
                .and_then(Value::as_u64),
            Some(12)
        );
        assert_eq!(
            result
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("fail")
        );
    }

    /// A gate stuck in verifying: its runner posted a verdict without token usage and never
    /// exited. The gate's time limit still ends it.
    #[test]
    fn an_llm_gate_whose_runner_never_exits_fails_at_its_time_limit() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "proof" state="ready" {
                goal "Complete mission proof."
                step "review" {
                  title "A held-out gate accepts the result"
                  gate "review" type="llm" {
                    model "claude-sonnet"
                    host "node"
                    workspace "."
                    tools "shell"
                    token-budget 10
                    time-limit "1s"
                    prompt "Inspect the result."
                  }
                }
              }

        "#;
        apply_source(&store, source, "mission-llm-hang");
        store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "proof".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-llm-hang".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let (runtime_id, result_subject) = {
            let members = runtime.started_members.lock().unwrap();
            let gate = members
                .iter()
                .find(|member| member.driver.as_deref() == Some("llm-gate"))
                .unwrap();
            (
                gate.runtime_id.clone(),
                gate.environment["ST_GATE_SUBJECT"].clone(),
            )
        };
        store
            .append_claim(&ClaimInput {
                subject: result_subject.clone(),
                kind: "gate.result".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    ("reason".into(), Value::String("it looks right".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("llm-posted-result".into()),
            })
            .unwrap();
        runtime.execs.lock().unwrap().insert(
            runtime_id.clone(),
            RuntimeObservation {
                runtime_id: runtime_id.clone(),
                terminal: false,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("gate-run".into()),
            },
        );
        reconciler.reconcile_once().unwrap();
        assert!(
            store
                .latest_claim(&result_subject, Some("gate.result"))
                .unwrap()
                .unwrap()
                .body
                .pointer("/fields/token_usage")
                .is_none(),
            "the gate settled before its time limit"
        );
        std::thread::sleep(Duration::from_millis(1_100));
        reconciler.reconcile_once().unwrap();
        let result = store
            .latest_claim(&result_subject, Some("gate.result"))
            .unwrap()
            .unwrap();
        assert_eq!(result.body["fields"]["verdict"], "fail");
        assert!(
            result.body["fields"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("exceeded")),
            "{result:#?}"
        );
        assert!(
            runtime.stops.lock().unwrap().contains(&runtime_id)
                || runtime.kills.lock().unwrap().contains(&runtime_id)
        );
    }

    #[test]
    fn adopts_a_matching_pty_without_a_start() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let intent =
            parse_intent("version 2\n agent \"worker\" { command \"true\" } ", "node").unwrap();
        let mission = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: "test".into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &mission.subject_tokens, "one")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("existing".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert!(runtime.starts.lock().unwrap().is_empty());
        let actual = store
            .latest_actual_value("agent/node.worker")
            .unwrap()
            .unwrap();
        assert_eq!(actual_field(&actual, "adopted"), Some(&Value::Bool(true)));
    }

    #[test]
    fn restart_types_follow_success_and_failure() {
        for (name, restart, exit_code, expected_starts) in [
            ("always-success", "always", 0, 2),
            ("failure-success", "on-failure", 0, 1),
            ("failure-error", "on-failure", 1, 2),
            ("never-error", "never", 1, 1),
        ] {
            let store = Arc::new(Store::open_memory("node").unwrap());
            let source = format!(
                r#"
                    version 2

                      agent "worker" {{
                        command "true"
                        restart "{restart}"
                      }}

                "#
            );
            apply_source(&store, &source, name);
            let runtime = Arc::new(FakeRuntime::default());
            let reconciler = Reconciler::new(
                store,
                runtime.clone(),
                "node".into(),
                Arc::new(Notify::new()),
            );

            reconciler.reconcile_once().unwrap();
            runtime.ptys.lock().unwrap().push(RuntimeObservation {
                runtime_id: "node.worker".into(),
                terminal: true,
                status: "exited".into(),
                exit_code: Some(exit_code),
                incarnation_id: Some(format!("{name}-one")),
            });
            reconciler.reconcile_once().unwrap();

            assert_eq!(
                runtime.starts.lock().unwrap().len(),
                expected_starts,
                "{name}"
            );
        }
    }

    #[test]
    fn restart_never_starts_a_new_desired_revision() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
                version 2

                  agent "worker" {
                    workspace "/tmp"
                    command "true"
                    env { REVISION "one" }
                    restart "never"
                  }

            "#,
            "member-one",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(0),
            incarnation_id: Some("worker-one".into()),
        });
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);

        apply_source(
            &store,
            r#"
                version 2

                  agent "worker" {
                    workspace "/tmp"
                    command "true"
                    env { REVISION "two" }
                    restart "never"
                  }

            "#,
            "member-two",
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();

        assert_eq!(runtime.starts.lock().unwrap().len(), 2);
        assert_eq!(
            runtime.started_members.lock().unwrap()[1].environment["REVISION"],
            "two"
        );
    }

    #[test]
    fn a_ding_is_an_explicit_child_runtime_not_a_reconciler_side_effect() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              agent "worker" {
                command "sleep 60"
                exec "ding" { command "true" }
              }
              message "one" {
                from "requester"
                to "node.worker"
                content "Do the work."
              }

        "#;
        apply_source(&store, source, "one-message");
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        });
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();

        assert!(
            runtime
                .started_members
                .lock()
                .unwrap()
                .iter()
                .any(|member| member.runtime_id == "exec.node.worker.ding")
        );
    }

    #[test]
    fn an_attention_request_closes_when_its_until_condition_holds_and_not_before() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
mission "publish" state="ready" {
  goal "Publish a revision."
  step "approve" { agentless }
}
"#,
            "until-mission",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "publish".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/nathan".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "until-run".into(),
            })
            .unwrap();
        let request = |subject: &str, until: Option<&str>| {
            store
                .request_attention_until(
                    subject,
                    &AttentionRequest {
                        reviewer: "person/nathan".into(),
                        title: "Publish this revision".into(),
                        reason: "Publish the prepared revision as a person.".into(),
                        severity: "warning".into(),
                        targets: vec![run.subject.clone()],
                        actor: "agent/node.requester".into(),
                        idempotency_key: format!("{subject}:requested"),
                    },
                    until,
                )
                .unwrap()
        };
        let until = request("attention/until-completed", Some("completed"));
        let plain = request("attention/plain", None);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let status = |subject: &str| store.attention_request(subject).unwrap().unwrap().status;

        reconciler.reconcile_once().unwrap();
        assert_eq!(status(&until.subject), "pending");

        store
            .set_mission_run_state(&run.id, "completed", "terminal", None)
            .unwrap();
        reconciler.reconcile_once().unwrap();

        let resolved = store.attention_request(&until.subject).unwrap().unwrap();
        assert_eq!(resolved.status, "resolved");
        assert_eq!(
            resolved.resolution_reason.as_deref(),
            Some("every target is completed")
        );
        assert_eq!(
            store
                .claims_for(&until.subject, Some("attention.resolved"))
                .unwrap()[0]
                .actor
                .as_deref(),
            Some("daemon/runtime")
        );
        assert_eq!(
            status(&plain.subject),
            "pending",
            "a request without until waits for a person"
        );
    }

    #[test]
    fn codex_start_failures_stop_after_three_attempts_and_request_one_attention() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
agent "worker" {
  workspace "."
  restart "always"
  harness "codex" { prompt "Wait for work." }
}
"#,
            "codex-start-failures",
        );
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .failed_starts
            .lock()
            .unwrap()
            .insert("node.worker".into());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.starts.lock().unwrap().len(), 3);
        let token = store
            .selected_desired_token("agent/node.worker")
            .unwrap()
            .unwrap();
        let key = format!("codex-crash-loop:agent/node.worker:{token}");
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention = store
            .attention_request(&format!("attention/{}", &digest[..32]))
            .unwrap()
            .unwrap();
        assert_eq!(attention.status, "pending");
        assert!(
            attention
                .reason
                .contains("the fake runtime rejected the start")
        );
        let decisions = store
            .claims_for("agent/node.worker", Some("runtime.reconcile-decision"))
            .unwrap();
        assert_eq!(
            decisions
                .iter()
                .filter(|claim| {
                    claim.body.pointer("/fields/key").and_then(Value::as_str)
                        == Some(format!("codex-crash-loop:{token}").as_str())
                })
                .count(),
            1
        );
    }

    #[test]
    fn codex_short_lived_exits_stop_relaunching() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
agent "worker" {
  workspace "."
  restart "always"
  harness "codex" { prompt "Wait for work." }
}
"#,
            "codex-short-lived-exits",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(17),
            incarnation_id: Some("bad-seat".into()),
        });
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.starts.lock().unwrap().len(), 3);
        let decision = store
            .latest_claim("agent/node.worker", Some("runtime.reconcile-decision"))
            .unwrap()
            .unwrap();
        assert_eq!(
            decision
                .body
                .pointer("/fields/decision")
                .and_then(Value::as_str),
            Some("raise")
        );
        assert!(
            decision
                .body
                .pointer("/fields/reason")
                .and_then(Value::as_str)
                .unwrap()
                .contains("exit code 17")
        );
    }

    #[test]
    fn fail_restart_intensity_parks_after_the_launch_budget() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              agent "worker" {
                command "true"
                restart "always"
                restart {
                  attempts 1
                  interval "60s"
                  mode "fail"
                }
              }

        "#;
        let intent = parse_intent(source, "node").unwrap();
        let mission = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &mission.subject_tokens, "one")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(1),
            incarnation_id: Some("first".into()),
        });
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        let actual = store
            .latest_actual_value("agent/node.worker")
            .unwrap()
            .unwrap();
        assert_eq!(
            actual_field(&actual, "reachability"),
            Some(&Value::String("unreachable".into()))
        );
        let desired_token = store
            .selected_desired_token("agent/node.worker")
            .unwrap()
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "runtime.restart-window-reset".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("desired_token".into(), Value::String(desired_token.clone())),
                    ("incarnation_id".into(), Value::String("other".into())),
                    ("reason".into(), Value::String("test the fence".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("wrong-incarnation-reset".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "runtime.restart-window-reset".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("desired_token".into(), Value::String(desired_token)),
                    ("incarnation_id".into(), Value::String("first".into())),
                    (
                        "reason".into(),
                        Value::String("clear this incarnation window".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("matching-incarnation-reset".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 2);
    }

    #[test]
    fn fail_restart_window_excludes_old_generations_even_without_a_matching_reset() {
        let now = 1_000_000_u128;
        let interval = 300_000_u64;
        assert!(!launch_in_restart_window(
            1,
            now - 700_000,
            0,
            now,
            interval,
            "fail"
        ));
        assert!(!launch_in_restart_window(
            2,
            now - 400_000,
            0,
            now,
            interval,
            "fail"
        ));
        assert!(launch_in_restart_window(
            3,
            now - 30_000,
            0,
            now,
            interval,
            "fail"
        ));
        assert!(!launch_in_restart_window(
            3,
            now - 30_000,
            3,
            now,
            interval,
            "fail"
        ));
    }

    #[test]
    fn three_exits_before_readiness_park_a_native_seat() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            "version 2\nagent \"worker\" { workspace \"/tmp\"; command \"true\"; restart \"always\" }\n",
            "unready-exits",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        for number in 1..=3 {
            *runtime.ptys.lock().unwrap() = vec![RuntimeObservation {
                runtime_id: "node.worker".into(),
                terminal: true,
                status: "exited".into(),
                exit_code: Some(1),
                incarnation_id: Some(format!("crash-{number}")),
            }];
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(runtime.starts.lock().unwrap().len(), 3);
        assert!(
            store
                .attention_requests(None, false)
                .unwrap()
                .iter()
                .any(|attention| { attention.targets.contains(&"agent/node.worker".to_owned()) })
        );
    }

    #[test]
    fn delay_restart_intensity_waits_for_the_sliding_window() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              agent "worker" {
                command "true"
                restart "always"
                restart {
                  attempts 1
                  interval "60s"
                  mode "delay"
                }
              }

        "#;
        let intent = parse_intent(source, "node").unwrap();
        let mission = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &mission.subject_tokens, "one")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(1),
            incarnation_id: Some("first".into()),
        });
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        let decision = store
            .latest_claim("agent/node.worker", Some("runtime.reconcile-decision"))
            .unwrap()
            .unwrap();
        assert_eq!(
            decision
                .body
                .pointer("/fields/decision")
                .and_then(Value::as_str),
            Some("wait")
        );
    }

    #[test]
    fn restart_delay_starts_at_the_exit_observation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              agent "worker" {
                command "true"
                restart "always"
                restart {
                  attempts 3
                  interval "60s"
                  delay "20ms"
                  mode "delay"
                }
              }

        "#;
        apply_source(&store, source, "restart-delay-from-exit");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        std::thread::sleep(Duration::from_millis(25));
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "exited".into(),
            exit_code: Some(1),
            incarnation_id: Some("first".into()),
        });
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);

        std::thread::sleep(Duration::from_millis(25));
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_stopped_agents_declared_checkout_is_read_once_per_declaration() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let declare = |command: &str| {
            let source =
                format!("version 2\n  agent \"worker\" {{\n    command \"{command}\"\n  }}\n");
            let intent = parse_intent(&source, "node").unwrap();
            let mission = store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.clone(),
                        source_name: None,
                    },
                )
                .unwrap();
            store
                .apply(
                    &intent,
                    &mission.subject_tokens,
                    &format!("declare {command}"),
                )
                .unwrap();
        };
        declare("sleep 60");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let parses = || DECLARATION_PARSES.with(std::cell::Cell::get);
        let before = parses();
        reconciler
            .declared_run_end_checkout("agent/node.worker")
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.other".into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([("status".into(), Value::String("running".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        reconciler
            .declared_run_end_checkout("agent/node.worker")
            .unwrap();
        assert_eq!(
            parses() - before,
            1,
            "an unrelated write must not re-read a stopped agent's declarations"
        );

        declare("sleep 61");
        reconciler
            .declared_run_end_checkout("agent/node.worker")
            .unwrap();
        assert_eq!(parses() - before, 2);
    }

    #[test]
    fn stop_waits_for_exit_and_then_kills_the_same_incarnation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let running_source = r#"
            version 2

              agent "worker" {
                command "sleep 60"
                shutdown-timeout "1ms"
              }

        "#;
        let running = parse_intent(running_source, "node").unwrap();
        let mission = store
            .mission(
                &running,
                crate::model::IntentInput {
                    kdl: running_source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&running, &mission.subject_tokens, "run")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("generation-one".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();

        let stop_source = r#"version 2
 stop "agent/node.worker" "#;
        let stop = parse_intent(stop_source, "node").unwrap();
        let mission = store
            .mission(
                &stop,
                crate::model::IntentInput {
                    kdl: stop_source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store.apply(&stop, &mission.subject_tokens, "stop").unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(&*runtime.stops.lock().unwrap(), &["node.worker"]);
        assert!(runtime.kills.lock().unwrap().is_empty());
        assert_ne!(
            actual_field(
                &store
                    .latest_actual_value("agent/node.worker")
                    .unwrap()
                    .unwrap(),
                "status"
            ),
            Some(&Value::String("stopped".into()))
        );

        std::thread::sleep(Duration::from_millis(2));
        reconciler.reconcile_once().unwrap();
        assert_eq!(&*runtime.kills.lock().unwrap(), &["node.worker"]);

        // The stop request, its deadline and the kill are this node's own records. They stay
        // local, and each cites the one before it.
        for kind in [
            "runtime.action.requested",
            "runtime.action.deadline-reached",
            "runtime.action.succeeded",
        ] {
            assert!(
                store
                    .claims_for("agent/node.worker", Some(kind))
                    .unwrap()
                    .is_empty(),
                "{kind} replicated"
            );
        }
        let request = store
            .observations_for("agent/node.worker", "runtime.action.requested")
            .unwrap()
            .into_iter()
            .find(|record| record.body["fields"]["action"] == "terminate")
            .unwrap();
        let deadline = store
            .latest_observation("agent/node.worker", "runtime.action.deadline-reached")
            .unwrap()
            .unwrap();
        assert_eq!(deadline.body["evidence"], serde_json::json!([request.id]));
        let kill = store
            .latest_observation("agent/node.worker", "runtime.action.succeeded")
            .unwrap()
            .unwrap();
        assert_eq!(kill.body["fields"]["action"], "kill");
        assert_eq!(kill.body["evidence"], serde_json::json!([deadline.id]));
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            runtime.kills.lock().unwrap().len(),
            1,
            "the local kill record fences a second kill"
        );
    }

    #[test]
    fn stop_does_not_kill_a_replacement_incarnation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let running_source = r#"
            version 2

              agent "worker" {
                command "sleep 60"
                shutdown-timeout "1ms"
              }

        "#;
        apply_source(&store, running_source, "replacement-run");
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: "node.worker".into(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("generation-one".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        apply_source(
            &store,
            r#"version 2
 stop "agent/node.worker" "#,
            "replacement-stop",
        );
        reconciler.reconcile_once().unwrap();

        runtime.ptys.lock().unwrap()[0].incarnation_id = Some("generation-two".into());
        std::thread::sleep(Duration::from_millis(2));
        reconciler.reconcile_once().unwrap();

        assert_eq!(runtime.stops.lock().unwrap().len(), 2);
        assert!(runtime.kills.lock().unwrap().is_empty());
    }

    fn scheduled_mission_revision(store: &Arc<Store>) -> String {
        apply_source(
            store,
            r#"version 2
mission "scheduled-cycle" state="ready" {
  completion { when "all-steps-exhausted" }
  goal "Complete one scheduled cycle."
  step "done" { agentless }
}"#,
            "scheduled-cycle",
        );
        store
            .mission_spec("scheduled-cycle", None)
            .unwrap()
            .unwrap()
            .revision
    }

    #[tokio::test]
    async fn one_time_schedule_starts_exactly_one_mission() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let at = (Utc::now() + chrono::Duration::milliseconds(50))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let source = format!(
            r#"
                version 2

                  schedule "reminder" {{
                    at "{at}"
                    work {{
                      mission "scheduled-cycle@{revision}"
                      workspace "/tmp/st3-schedule-test"
                    }}
                  }}

            "#
        );
        apply_source(&store, &source, "schedule-one");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let schedule_claims = store.claims_for("schedule/reminder", None).unwrap();
        assert_eq!(
            schedule_claims
                .iter()
                .filter(|claim| claim.kind == "schedule.occurrence-reached")
                .count(),
            1,
            "{schedule_claims:#?}"
        );
        assert_eq!(
            schedule_claims
                .iter()
                .filter(|claim| claim.kind == "schedule.work-started")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_recurring_schedule_starts_distinct_ordered_occurrences() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let anchor = (Utc::now() + chrono::Duration::milliseconds(40))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let source = format!(
            r#"version 2
 schedule "cycle" {{
   every "60ms"
   anchor "{anchor}"
   catch-up "latest"
   work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp/st3-schedule-test" }}
 }}"#
        );
        apply_source(&store, &source, "recurring-schedule");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(55)).await;
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(65)).await;
        for _ in 0..10 {
            reconciler.reconcile_once().unwrap();
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        reconciler.reconcile_once().unwrap();

        let reached = store
            .claims_for("schedule/cycle", Some("schedule.occurrence-reached"))
            .unwrap();
        let occurrences = reached
            .iter()
            .map(|claim| {
                claim
                    .body
                    .pointer("/fields/occurrence")
                    .and_then(Value::as_u64)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            occurrences.len() >= 2,
            "the recurring schedule did not start twice: {occurrences:?}"
        );
        assert!(
            occurrences.windows(2).all(|pair| pair[0] < pair[1]),
            "recurring occurrences were not distinct and ordered: {occurrences:?}"
        );
        assert_eq!(
            store
                .claims_for("schedule/cycle", Some("schedule.work-started"))
                .unwrap()
                .len(),
            occurrences.len()
        );
    }

    #[test]
    fn a_stopped_schedule_cancels_its_queued_work() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let declared = format!(
            r#"version 2
 schedule "held" {{
   every "1h"
   anchor "2030-01-01T00:00:00Z"
   work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp/st3-schedule-test" }}
 }}"#
        );
        apply_source(&store, &declared, "held-schedule");
        let schedule_revision = store
            .selected_desired_revision("schedule/held")
            .unwrap()
            .unwrap();
        let request = store
            .append_claim(&ClaimInput {
                subject: "schedule/held".into(),
                kind: "schedule.work-requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("revision".into(), Value::String(schedule_revision)),
                    ("occurrence".into(), Value::from(0)),
                    (
                        "mission".into(),
                        Value::String("mission/scheduled-cycle".into()),
                    ),
                    ("mission_revision".into(), Value::String(revision)),
                    (
                        "workspace".into(),
                        Value::String("/tmp/st3-schedule-test".into()),
                    ),
                    ("inputs".into(), serde_json::json!({})),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        apply_source(
            &store,
            "version 2\nschedule \"held\" { stop }\n",
            "stop-held-schedule",
        );
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        let failed = store
            .claims_for("schedule/held", Some("schedule.work-failed"))
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].body["fields"]["request"], request.id.as_str());
        assert_eq!(failed[0].body["fields"]["code"], "schedule-stopped");

        // Declared again, the schedule does not start the work queued before it stopped.
        apply_source(&store, &declared, "held-schedule-again");
        reconciler.reconcile_once().unwrap();
        assert!(
            store
                .claims_for("schedule/held", Some("schedule.work-started"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn all_catch_up_beyond_its_maximum_holds_with_a_fault_on_the_schedule() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let anchor = (Utc::now() - chrono::Duration::seconds(10))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let source = format!(
            r#"version 2
 schedule "burst" {{
   every "1s"
   anchor "{anchor}"
   catch-up "all"
   max-catch-up 3
   work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp/st3-schedule-test" }}
 }}"#
        );
        apply_source(&store, &source, "burst-schedule");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .tolerating_faults();

        reconciler.reconcile_once().unwrap();

        let fault = store
            .reconcile_fault("schedule/burst", "schedule")
            .unwrap()
            .expect("the held schedule recorded no fault");
        assert!(fault.contains("max-catch-up 3"), "{fault}");
        assert!(
            store
                .claims_for("schedule/burst", Some("schedule.occurrence-scheduled"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn latest_catch_up_starts_only_the_current_missed_occurrence() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let anchor = (Utc::now() - chrono::Duration::seconds(10))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let source = format!(
            r#"version 2
 schedule "cycle" {{
   every "1s"
   anchor "{anchor}"
   catch-up "latest"
   work {{ mission "scheduled-cycle@{revision}"; workspace "/tmp/st3-schedule-test" }}
 }}"#
        );
        apply_source(&store, &source, "latest-catch-up");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let reached = store
            .claims_for("schedule/cycle", Some("schedule.occurrence-reached"))
            .unwrap();
        assert_eq!(reached.len(), 1);
        assert!(
            reached[0]
                .body
                .pointer("/fields/occurrence")
                .and_then(Value::as_u64)
                .unwrap()
                >= 9
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store
                .claims_for("schedule/cycle", Some("schedule.work-started"))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_new_schedule_revision_cancels_the_armed_wake() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let revision = scheduled_mission_revision(&store);
        let at = (Utc::now() + chrono::Duration::milliseconds(100))
            .to_rfc3339_opts(SecondsFormat::Millis, true);
        let source = format!(
            r#"
                version 2

                  schedule "reminder" {{
                    at "{at}"
                    work {{
                      mission "scheduled-cycle@{revision}"
                      workspace "/tmp/st3-schedule-test"
                    }}
                  }}

            "#
        );
        apply_source(&store, &source, "schedule-arm");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();

        apply_source(
            &store,
            r#"version 2
 schedule "reminder" { stop } "#,
            "schedule-stop",
        );
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert!(
            store
                .claims_for("schedule/reminder", Some("schedule.occurrence-reached"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .claims_for("schedule/reminder", Some("schedule.occurrence-cancelled"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .claims_for("schedule/reminder", Some("schedule.work-started"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn removed_link_nodes_are_rejected() {
        let source = r#"
            version 2

              agent "source" { command "true" }
              agent "target" { command "true" }
              link "dependency" {
                from "agent/node.source"
                to "agent/node.target"
              }

        "#;
        let error = parse_intent(source, "node").unwrap_err();
        assert_eq!(error.code, "unknown-node");
    }

    #[test]
    fn removed_supervisor_nodes_are_rejected() {
        let source = r#"
            version 2

              supervisor "watch" {
                terminal-control "confirmation" driver="codex" {
                  contains "Press Enter"
                  key "enter"
                  max-inputs 1
                }
              }
              agent "worker" {
                supervisor "watch"
                harness "codex" { prompt "Do the work." }
              }

        "#;
        let error = parse_intent(source, "node").unwrap_err();
        assert_eq!(error.code, "unknown-node");
    }

    #[tokio::test]
    async fn only_claimed_execution_consumes_a_step_timeout_and_failure_selects_cleanup() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "eval/demo" state="ready" timeout="1m" {
                goal "Complete mission eval/demo."
                agent "worker" { workspace "/tmp"; command "true"; restart "never" }
                step "result" timeout="1ms" {
                  assigned-to "agent/${ST_MISSION_RUN}/worker"
                  title "The result appears"
                }
                step "after" {
                  agentless
                  title "Failed work never releases this step"
                  depends-on { step "result" completed }
                }
                completion { when "all-steps-exhausted" }
                finally {
                  step "cleanup" {
                    agentless
                    title "The final work completes"
                  }
                }
              }
        "#;
        apply_source(&store, source, "mission-cleanup");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "eval/demo".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-cleanup".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        reconciler.reconcile_once().unwrap();
        let waiting = store.mission_run(&run.id).unwrap().unwrap();
        let result = waiting
            .steps
            .iter()
            .find(|step| step.step == "result")
            .unwrap();
        assert_eq!(result.status, "ready");
        assert_eq!(result.execution_started_at_unix_ms, None);
        assert_eq!(result.execution_elapsed_ms, 0);
        store
            .work_action(
                &result.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: result.assigned_to.clone(),
                    incarnation: Some("worker-one".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-timeout-work".into(),
                },
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        for _ in 0..20 {
            reconciler.reconcile_once().unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.phase, "terminal");
        assert_eq!(
            run.steps
                .iter()
                .find(|step| step.step == "cleanup")
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            store
                .latest_claim(&run.subject, Some("eval.verdict"))
                .unwrap()
                .unwrap()
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("fail")
        );
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|desired| { desired.owner_run.as_deref() != Some(run.subject.as_str()) })
        );
    }

    #[test]
    fn a_retried_attempt_gets_its_own_gate_result() {
        let stage = |attempt| GateContext {
            subject: "step-run/generation/window".into(),
            name: "window".into(),
            started_at_unix_ms: 0,
            attempt,
        };
        let definition = serde_json::json!({"type": "mechanical", "command": "/usr/bin/true"});
        let first = gate_result_subject(&stage(1), "open", &definition).unwrap();
        // The first attempt keeps the key that earlier releases recorded results under.
        assert_eq!(
            first,
            gate_operation_subject(&stage(1), "open", &definition).unwrap()
        );
        let second = gate_result_subject(&stage(2), "open", &definition).unwrap();
        let third = gate_result_subject(&stage(3), "open", &definition).unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert!(second.starts_with("gate-operation/step-run.generation.window/"));
        assert_eq!(
            second,
            gate_result_subject(&stage(2), "open", &definition).unwrap()
        );
    }

    #[tokio::test]
    async fn a_run_started_after_another_waits_until_that_run_completes() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        apply_source(
            &store,
            r#"
            version 2

              agent "example/shipper" { workspace "/tmp"; command "true"; restart "never" }
              mission "build" state="ready" {
                goal "Build until the test releases the hold."
                concurrent-runs
                step "hold" {
                  agentless
                  gate "released" { field "state" "resource/never" is "ready" }
                }
              }
              mission "ship" state="ready" {
                goal "Ship after the build."
                concurrent-runs
                step "ship" { assigned-to "agent/example/shipper" }
              }
            "#,
            "after-missions",
        );
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let start = |run: &str, mission: &str, after: Option<&str>| {
            let revision = store.mission_spec(mission, None).unwrap().unwrap().revision;
            let after = after.map_or(String::new(), |after| format!("  after {after:?}\n"));
            let source = format!(
                "version 2\nmission-run {run:?} {{\n  mission \"mission/{mission}@{revision}\"\n  workspace {:?}\n  requester \"person/operator\"\n{after}}}\n",
                workspace.path().display().to_string(),
            );
            let intent = parse_intent(&source, "node").unwrap();
            let preview = store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source.clone(),
                        source_name: None,
                    },
                )
                .unwrap();
            assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
            store
                .apply_as(
                    &intent,
                    &preview.subject_tokens,
                    &format!("start-{run}"),
                    Some("person/operator"),
                )
                .unwrap();
        };
        let step = |run: &str, path: &str| {
            store
                .mission_run(run)
                .unwrap()
                .unwrap()
                .steps
                .into_iter()
                .find(|step| step.step == path)
                .unwrap()
        };

        start("build/1", "build", None);
        start("ship/1", "ship", Some("build/1"));
        let waiting = store.mission_run("ship/1").unwrap().unwrap();
        assert_eq!(waiting.after.as_deref(), Some("mission-run/build/1"));
        assert_eq!(waiting.steps[0].step, "after-run");
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let after = step("ship/1", "after-run");
        assert_eq!(after.status, "working");
        assert_eq!(
            after.blocked_reason.as_deref(),
            Some("waiting for `mission-run/build/1` to complete")
        );
        assert_eq!(step("ship/1", "ship").status, "pending");
        let queue = store.seat_queue("agent/example/shipper").unwrap();
        assert_eq!(queue.runs[0].state, "waiting");
        assert_eq!(
            queue.runs[0].waiting_for.as_deref(),
            Some("mission-run/build/1")
        );

        store
            .set_step_state(&step("build/1", "hold").subject, "completed", None)
            .unwrap();
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run("build/1").unwrap().unwrap().status,
            "completed"
        );
        assert_eq!(step("ship/1", "after-run").status, "completed");
        assert_eq!(step("ship/1", "ship").status, "ready");
        let queue = store.seat_queue("agent/example/shipper").unwrap();
        assert_eq!(queue.runs[0].waiting_for, None);

        start("build/2", "build", None);
        start("ship/2", "ship", Some("mission-run/build/2"));
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        store
            .set_step_state(
                &step("build/2", "hold").subject,
                "failed",
                Some("test failure"),
            )
            .unwrap();
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        let after = step("ship/2", "after-run");
        assert_eq!(after.status, "failed");
        assert_eq!(
            after.blocked_reason.as_deref(),
            Some("the awaited mission run `mission-run/build/2` is failed")
        );
        assert_eq!(step("ship/2", "ship").status, "cancelled");
        assert_eq!(
            store.mission_run("ship/2").unwrap().unwrap().status,
            "failed"
        );
    }

    #[tokio::test]
    async fn agentless_waits_open_an_execution_interval_and_time_out() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              resource "never" { kind "custom.test.never-ready" }
              mission "agentless-timeout" state="ready" timeout="1m" {
                goal "Bound an agentless wait."
                completion { when "all-steps-exhausted" }
                step "wait" timeout="1ms" {
                  agentless
                  gate "the absent resource becomes ready" {
                    field "state" "resource/never" is "ready"
                  }
                }
              }
        "#;
        apply_source(&store, source, "agentless-timeout-source");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "agentless-timeout".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "agentless-timeout-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let active = store.mission_run(&run.id).unwrap().unwrap();
        let wait = active
            .steps
            .iter()
            .find(|step| step.step == "wait")
            .unwrap();
        assert_eq!(wait.status, "working");
        assert!(wait.execution_started_at_unix_ms.is_some());
        // Starting agentless execution is not a blocker, so it records no reason.
        assert_eq!(wait.blocked_reason, None);

        tokio::time::sleep(Duration::from_millis(5)).await;
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        let terminal = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(terminal.status, "failed");
        assert_eq!(terminal.phase, "terminal");
        assert_eq!(
            terminal
                .steps
                .iter()
                .find(|step| step.step == "wait")
                .unwrap()
                .status,
            "failed"
        );
    }

    #[tokio::test]
    async fn one_cancellation_wake_converges_through_runtime_cleanup() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "cancel-convergence" state="ready" timeout="1m" {
                goal "Keep one worker ready until cancellation."
                agent "worker" { workspace "/tmp"; command "true"; restart "never" }
                step "wait" {
                  assigned-to "agent/${ST_MISSION_RUN}/worker"
                  goal "Wait for cancellation."
                }
                finally {
                  step "cleanup" { agentless }
                }
              }

        "#;
        apply_source(&store, source, "cancel-convergence-source");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "cancel-convergence".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "cancel-convergence-run".into(),
            })
            .unwrap();
        let notify = Arc::new(Notify::new());
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            notify.clone(),
        ));
        let task = tokio::spawn(reconciler.run());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if runtime
                    .started_members
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|member| member.runtime_id.ends_with(".worker"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the worker did not start");

        let cancellation = format!(
            "version 2\nmission-run {:?} {{ cancellation \"operator-stop\" {{ reason \"the test ended\" }} }}\n",
            run.id
        );
        apply_source_beside_the_loop(&store, &cancellation, "cancel-convergence-stop");
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let current = beside_the_loop(|| store.mission_run(&run.id)).unwrap();
                let cleaned = beside_the_loop(|| store.desired_subjects())
                    .iter()
                    .all(|desired| desired.owner_run.as_deref() != Some(run.subject.as_str()));
                if current.status == "cancelled" && current.phase == "terminal" && cleaned {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("one cancellation wake did not finish cleanup");
        task.abort();

        assert!(
            beside_the_loop(|| store.desired_subjects())
                .iter()
                .all(|desired| desired.owner_run.as_deref() != Some(run.subject.as_str()))
        );
    }

    #[tokio::test]
    async fn a_declared_checkout_exists_before_its_agent_starts_and_leaves_with_its_run() {
        let root = tempfile::tempdir().unwrap();
        let repository = crate::checkout::test_support::repository(root.path());
        let workspace = root.path().join("worker");
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = format!(
            r#"
            version 2

              mission "checkout-lifecycle" state="ready" timeout="1m" {{
                goal "Work in a worktree that st creates and removes."
                agent "worker" {{
                  workspace {workspace:?}
                  checkout {repository:?} base="origin/main" branch="example/${{ST_MISSION_RUN}}" remove-at-run-end=#true
                  command "true"
                  restart "never"
                }}
                step "wait" {{
                  assigned-to "agent/${{ST_MISSION_RUN}}/worker"
                  goal "Wait for cancellation."
                }}
              }}
            "#,
            workspace = workspace.display().to_string(),
            repository = repository.display().to_string(),
        );
        apply_source(&store, &source, "checkout-lifecycle-source");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "checkout-lifecycle".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "checkout-lifecycle-run".into(),
            })
            .unwrap();
        let notify = Arc::new(Notify::new());
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            notify.clone(),
        ));
        let task = tokio::spawn(reconciler.run());

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if runtime
                    .started_members
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|member| member.runtime_id.ends_with(".worker"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the worker did not start");
        let branch = std::process::Command::new("git")
            .arg("-C")
            .arg(&workspace)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&branch.stdout).trim(),
            format!("example/{}", run.id),
            "the worktree existed on its branch when the worker started"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("README")).unwrap(),
            "second\n",
            "the checkout started from the fetched base"
        );

        let cancellation = format!(
            "version 2\nmission-run {:?} {{ cancellation \"operator-stop\" {{ reason \"the test ended\" }} }}\n",
            run.id
        );
        apply_source_beside_the_loop(&store, &cancellation, "checkout-lifecycle-stop");
        notify.notify_one();
        tokio::time::timeout(Duration::from_secs(10), async {
            while workspace.exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the finished run did not remove its checkout");
        task.abort();
        crate::checkout::test_support::git(
            &repository,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/example/{}", run.id),
            ],
        );
    }

    #[test]
    fn a_finished_checkout_stays_while_in_use_or_changed() {
        let root = tempfile::tempdir().unwrap();
        let repository = crate::checkout::test_support::repository(root.path());
        let workspace = root.path().join("shared");
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; checkout {:?} base=\"main\" branch=\"example/shared\" remove-at-run-end=#true; command \"true\"; restart \"never\" }}\n",
            workspace.display().to_string(),
            repository.display().to_string(),
        );
        apply_source(&store, &source, "checkout-shared");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert!(workspace.join("README").is_file());
        assert_eq!(runtime.started_members.lock().unwrap().len(), 1);

        let agent = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.kind == "agent")
            .unwrap();
        let checkout = Checkout::from_desired(&agent.desired).unwrap();
        let path = workspace.display().to_string();
        reconciler
            .remove_finished_checkout(
                &agent.subject,
                &checkout,
                &workspace,
                &BTreeSet::from([path.as_str()]),
            )
            .unwrap();
        assert!(
            workspace.is_dir(),
            "a current member still uses the workspace"
        );

        std::fs::write(workspace.join("notes.txt"), "unfinished\n").unwrap();
        reconciler
            .remove_finished_checkout(&agent.subject, &checkout, &workspace, &BTreeSet::new())
            .unwrap();
        assert!(workspace.join("notes.txt").is_file(), "changed work stays");
        let kept = store
            .latest_claim(&agent.subject, Some("harness.diagnostic"))
            .unwrap()
            .unwrap();
        assert_eq!(
            kept.body.pointer("/fields/code").and_then(Value::as_str),
            Some("checkout-kept")
        );

        std::fs::remove_file(workspace.join("notes.txt")).unwrap();
        reconciler.checkout_retries.lock().unwrap().clear();
        reconciler
            .remove_finished_checkout(&agent.subject, &checkout, &workspace, &BTreeSet::new())
            .unwrap();
        assert!(!workspace.exists());
    }

    #[test]
    fn a_failed_checkout_keeps_its_agent_from_starting() {
        let root = tempfile::tempdir().unwrap();
        let repository = crate::checkout::test_support::repository(root.path());
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; checkout {:?} base=\"no-such-base\" branch=\"example/missing\"; command \"true\" }}\n",
            root.path().join("missing").display().to_string(),
            repository.display().to_string(),
        );
        apply_source(&store, &source, "checkout-missing-base");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();

        assert!(runtime.started_members.lock().unwrap().is_empty());
        let subject = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.kind == "agent")
            .unwrap()
            .subject;
        let diagnostics = store
            .claims_for(&subject, Some("runtime.reconcile-decision"))
            .unwrap();
        assert_eq!(
            diagnostics.len(),
            1,
            "a retry within the backoff adds nothing"
        );
        assert_eq!(
            diagnostics[0]
                .body
                .pointer("/fields/decision")
                .and_then(Value::as_str),
            Some("member-fault")
        );
        assert!(
            diagnostics[0]
                .body
                .pointer("/fields/reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.contains("checkout of")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn an_expired_run_cancels_ready_work_before_cleanup() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
mission "ready-deadline" state="ready" timeout="1ms" {
  goal "Expire while assigned work is ready."
  step "work" { assigned-to "agent/absent" }
}
"#,
            "ready-deadline-source",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "ready-deadline".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "ready-deadline-run".into(),
            })
            .unwrap();
        store
            .set_step_state(&run.steps[0].subject, "ready", None)
            .unwrap();
        std::thread::sleep(Duration::from_millis(3));
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let current = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(current.status, "failed");
        assert_eq!(current.phase, "terminal");
        assert_eq!(current.steps[0].status, "cancelled");
    }

    #[tokio::test]
    async fn cleanup_does_not_wait_for_a_stop_target_that_never_started() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
mission "absent-stop" state="ready" {
  goal "Finish with an absent stop target."
  completion { when "all-steps-exhausted" }
  step "work" { agentless }
  finally { step "stop" { agentless; stop "agent/absent" } }
}
"#,
            "absent-stop-source",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "absent-stop".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "absent-stop-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..100 {
            reconciler.reconcile_once().unwrap();
            if store.mission_run(&run.id).unwrap().unwrap().phase == "terminal" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let current = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(current.status, "completed");
        assert_eq!(current.phase, "terminal");
    }

    #[tokio::test]
    async fn the_daemon_wakes_at_a_mission_deadline_and_finishes_eval_cleanup() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              mission "eval/deadline" state="ready" timeout="50ms" {
                goal "Fail and clean up at the daemon-owned deadline."
                completion { when "all-steps-exhausted" }
                step "wait" {
                  agentless
                  gate "never" { field "status" "resource/never" "is" "ready" }
                }
              }
              mission "eval/deadline/helper" state="ready" {
                goal "Remain open until the root eval deadline expires."
                step "wait" {
                  agentless
                  gate "never" { field "status" "resource/never" "is" "ready" }
                }
              }
              resource "never" { kind "custom.test.deadline" }

        "#;
        apply_source(&store, source, "mission-deadline-source");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "eval/deadline".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "mission-deadline-run".into(),
            })
            .unwrap();
        let child = store
            .create_child_mission_run(
                &crate::model::MissionRunRequest {
                    mission: "eval/deadline/helper".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("daemon/runtime".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "mission-deadline-child".into(),
                },
                &run,
                &run.steps[0].subject,
                None,
            )
            .unwrap();
        let reconciler = Arc::new(Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        ));
        let task = tokio::spawn(reconciler.run());

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let current = beside_the_loop(|| store.mission_run(&run.id)).unwrap();
                if current.status == "failed" && current.phase == "terminal" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the daemon did not wake at the mission deadline");
        task.abort();

        let verdict =
            beside_the_loop(|| store.latest_claim(&run.subject, Some("eval.verdict"))).unwrap();
        assert_eq!(
            verdict
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("fail")
        );
        assert_eq!(
            verdict
                .body
                .pointer("/fields/reason")
                .and_then(Value::as_str),
            Some("the mission timeout expired after 50ms")
        );
        let child = beside_the_loop(|| store.mission_run(&child.id)).unwrap();
        assert_eq!(child.status, "cancelled");
        assert_eq!(child.phase, "terminal");
        assert!(
            beside_the_loop(|| store.desired_subjects())
                .iter()
                .all(|desired| desired.owner_run.as_deref() != Some(run.subject.as_str()))
        );
    }

    #[test]
    fn a_satisfied_dependency_predicate_stays_latched() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
            version 2

              resource "approval" { kind "human.review" }
              agent "worker" { workspace "/tmp"; command "true"; restart "never" }
              mission "release" state="ready" {
                goal "Complete mission release."
                step "publish" {
                  assigned-to "agent/worker"
                  depends-on {
                    field "decision" "resource/approval" "is" "approve"
                  }
                }
              }

        "#;
        apply_source(&store, source, "latched-dependency");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "release".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-latched-dependency".into(),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "resource/approval".into(),
                kind: "resource.observed".into(),
                actor: Some("person/reviewer".into()),
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("human.review".into())),
                    ("decision".into(), Value::String("approve".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("approve".into()),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "ready"
        );

        store
            .append_claim(&ClaimInput {
                subject: "resource/approval".into(),
                kind: "resource.observed".into(),
                actor: Some("person/reviewer".into()),
                fields: BTreeMap::from([(
                    "decision".into(),
                    Value::String("request-changes".into()),
                )]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("reject".into()),
            })
            .unwrap();

        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().steps[0].status,
            "ready"
        );
    }

    #[test]
    fn a_mission_completes_when_it_has_no_next_step() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "standing" state="ready" {
    goal "Complete when its work is exhausted."
    step "prepare" { agentless }
  }

"#;
        apply_source(&store, source, "publish-standing");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "standing".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-standing".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.steps[0].status, "completed");
        assert_eq!(run.status, "completed");

        let zero_source = r#"
version 2
 mission "zero" state="ready" { goal "Complete with no steps." }
"#;
        apply_source(&store, zero_source, "publish-zero");
        let zero = store
            .create_mission_run(&MissionRunRequest {
                mission: "zero".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-zero".into(),
            })
            .unwrap();
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&zero.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn a_first_class_loop_runs_bounded_child_missions_until_its_gate_passes() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

resource "loop-result" { kind "custom.test.loop-result" }
mission "loop" state="ready" {
  goal "Repeat a bounded graph until its result is ready."
  input "expected" kind="text"
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 3
    until { gate "ready" { field "state" "resource/loop-result" is "${input.expected}" } }
    round {
      completion { when "all-steps-exhausted" }
      step "work" {
        agentless
        goal "Complete round ${loop.round}."
      }
    }

  }
}
"#;
        apply_source(&store, source, "first-class-loop");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "loop".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::from([("expected".into(), "ready".into())]),
                idempotency_key: "first-class-loop-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..12 {
            reconciler.reconcile_once().unwrap();
        }
        let first = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(first.steps[0].attempt, 2);
        assert_ne!(first.status, "completed");
        let second_round = store
            .mission_run(&store.mission_run_subject_for_idempotency_key(&format!(
                "loop-round:{}:2",
                first.steps[0].subject
            )))
            .unwrap()
            .unwrap();
        assert_eq!(second_round.inputs[LOOP_ROUND_INPUT].value, "2");

        store
            .append_claim(&ClaimInput {
                subject: "resource/loop-result".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    (
                        "kind".into(),
                        Value::String("custom.test.loop-result".into()),
                    ),
                    ("state".into(), Value::String("ready".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("loop-result-ready".into()),
            })
            .unwrap();
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        let completed = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.loops.len(), 1);
        assert_eq!(completed.loops[0].status, "completed");
        assert_eq!(completed.loops[0].round, 2);
        assert_eq!(completed.loops[0].results.len(), 2);
    }

    #[test]
    fn an_unclaimed_timed_out_round_is_redispatched_without_spending_a_round() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "worker" { workspace "/tmp"; command "true"; restart "never" }
mission "unclaimed-loop" state="ready" {
  goal "Do not call scheduling failure an execution failure."
  completion { when "all-steps-exhausted" }
  loop "improve" timeout="20ms" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "work" { assigned-to "agent/node.worker" }
    }
  }
}
"#;
        apply_source(&store, source, "unclaimed-loop-source");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "unclaimed-loop".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "unclaimed-loop-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }
        std::thread::sleep(Duration::from_millis(25));
        let loop_subject = format!(
            "loop-run/{}/improve",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        for _ in 0..12 {
            reconciler.reconcile_once().unwrap();
            if !store
                .claims_for(&loop_subject, Some("loop.round-dispatch"))
                .unwrap()
                .is_empty()
            {
                break;
            }
        }

        let current = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(current.steps[0].attempt, 1);
        assert!(
            !matches!(current.status.as_str(), "failed" | "cancelled"),
            "unexpected parent state: status={} phase={} step={} reason={:?}",
            current.status,
            current.phase,
            current.steps[0].status,
            current.steps[0].blocked_reason
        );
        assert!(
            store
                .claims_for(&loop_subject, Some("loop.round-result"))
                .unwrap()
                .is_empty(),
            "a never-claimed dispatch is not a loop result"
        );
        let dispatches = store
            .claims_for(&loop_subject, Some("loop.round-dispatch"))
            .unwrap();
        assert_eq!(dispatches.len(), 1);
        assert_eq!(
            dispatches[0]
                .body
                .pointer("/fields/dispatch")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            dispatches[0]
                .body
                .pointer("/fields/reason")
                .and_then(Value::as_str),
            Some(
                "the round mission timeout expired before any worker claimed its work; rescheduled without consuming max-rounds"
            )
        );
    }

    #[test]
    fn loop_stop_rules_use_durable_round_results() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
mission "stops" state="ready" {
  goal "Evaluate durable loop stop rules."
  loop "improve" {
    max-rounds 5
    metric "quality" direction="higher" min-improvement=0.1 {
      field "score" "resource/result"
    }
    stop { plateau metric="quality" rounds=2; repeated-failure 2; token-budget 100 }
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let spec = intent.missions["stops"].steps["improve"]
            .loop_spec
            .as_ref()
            .unwrap()
            .as_ref()
            .clone();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let subject = "loop-run/01990000000070008000000000000000/improve";
        for (round, quality) in [(1, 1.0), (2, 1.05), (3, 1.06)] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "loop.round-result".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("round".into(), Value::from(round)),
                        ("status".into(), Value::String("completed".into())),
                        (
                            "mission_run".into(),
                            Value::String(format!("mission-run/round-{round}")),
                        ),
                        ("metrics".into(), serde_json::json!({"quality": quality})),
                        ("token_usage".into(), Value::from(10)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("stop-round-{round}")),
                })
                .unwrap();
        }
        assert_eq!(
            reconciler.loop_stop_reason(subject, &spec).unwrap(),
            Some("the loop metric did not improve for 2 rounds".into())
        );

        let mut token_spec = spec.clone();
        token_spec.stop.plateau_metric = None;
        token_spec.stop.plateau_rounds = None;
        token_spec.stop.token_budget = Some(30);
        assert_eq!(
            reconciler.loop_stop_reason(subject, &token_spec).unwrap(),
            Some("the loop token budget of 30 was reached".into())
        );

        for round in 4..=5 {
            store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "loop.round-result".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("round".into(), Value::from(round)),
                        ("status".into(), Value::String("failed".into())),
                        (
                            "mission_run".into(),
                            Value::String(format!("mission-run/round-{round}")),
                        ),
                        ("metrics".into(), serde_json::json!({})),
                        ("token_usage".into(), Value::from(0)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("stop-failure-{round}")),
                })
                .unwrap();
        }
        assert_eq!(reconciler.repeated_loop_failures(subject).unwrap(), 2);
    }

    #[test]
    #[should_panic(expected = "removed-loop-shape")]
    fn a_removed_for_each_loop_is_rejected_before_reconciliation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
resource "batch" { kind "custom.test.batch" }
mission "gauntlet" state="ready" {
  goal "Run one child graph for each stable input item."
  completion { when "all-steps-exhausted" }
  loop "checks" for-each="resource/batch" field="items" max-parallel=2 {
    max-rounds 5
    round {
      completion { when "all-steps-exhausted" }
      step "check" { agentless; goal "Check ${loop.item.name}." }
    }
  }
}
"#;
        apply_source(&store, source, "for-each-loop");
        store
            .append_claim(&ClaimInput {
                subject: "resource/batch".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("custom.test.batch".into())),
                    (
                        "items".into(),
                        serde_json::json!([
                            {"id":"one","name":"first"}, {"id":"two","name":"second"},
                            {"id":"three","name":"third"}
                        ]),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("batch-items".into()),
            })
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "gauntlet".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "gauntlet-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..40 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
        let subject = format!(
            "loop-run/{}/checks",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        assert_eq!(
            store
                .claims_for(&subject, Some("loop.round-result"))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    #[should_panic(expected = "removed-loop-shape")]
    fn a_removed_malformed_for_each_loop_is_rejected_before_reconciliation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
resource "batch" { kind "custom.test.batch" }
mission "gauntlet" state="ready" {
  goal "Contain an invalid input snapshot."
  completion { when "all-steps-exhausted" }
  loop "checks" for-each="resource/batch" field="items" {
    max-rounds 5
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
        apply_source(&store, source, "malformed-for-each-loop");
        store
            .append_claim(&ClaimInput {
                subject: "resource/batch".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), Value::String("custom.test.batch".into())),
                    ("items".into(), Value::String("not-an-array".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("invalid-batch-items".into()),
            })
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "gauntlet".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "malformed-gauntlet-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }

        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.steps[0].status, "cancelled");
        assert_eq!(
            run.steps[0].blocked_reason.as_deref(),
            Some("a for-each field must contain an array")
        );
    }

    #[test]
    fn a_human_can_accept_the_best_result_after_loop_exhaustion() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
resource "result" { kind "custom.test.loop-result" }
mission "human-exhaustion" state="ready" {
  goal "Let a person accept the bounded result."
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 1
    until { gate "ready" { field "state" "resource/result" is "ready" } }
    round { completion { when "all-steps-exhausted" } }
    on-exhausted {
      gate "accept-best" type="human" {
        reviewer "person/nathan"
        question "Accept the best bounded result?"
      }
    }
  }
}
"#;
        apply_source(&store, source, "human-loop-exhaustion");
        store
            .append_claim(&ClaimInput {
                subject: "resource/result".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    (
                        "kind".into(),
                        Value::String("custom.test.loop-result".into()),
                    ),
                    ("state".into(), Value::String("not-ready".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("human-loop-result".into()),
            })
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "human-exhaustion".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "human-exhaustion-run".into(),
            })
            .unwrap();
        let loop_subject = format!(
            "loop-run/{}/improve",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let mut requested = None;
        for _ in 0..20 {
            reconciler.reconcile_once().unwrap();
            if let Some(request) = store.gate_request_for_owner(&loop_subject).unwrap() {
                requested = Some(request);
                break;
            }
        }
        let request = requested.expect("the exhaustion review was not requested");
        assert_eq!(
            request
                .body
                .pointer("/fields/question")
                .and_then(Value::as_str),
            Some("Accept the best bounded result?")
        );
        store
            .append_claim(&ClaimInput {
                subject: request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/nathan".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    ("request".into(), Value::String(request.id.clone())),
                ]),
                evidence: vec![request.id],
                expected_subject: None,
                idempotency_key: Some("accept-human-loop-result".into()),
            })
            .unwrap();
        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }

        let completed = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.loops[0].status, "exhausted");
    }

    #[test]
    fn a_loop_waiting_on_its_gate_writes_its_state_once() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
mission "gated-loop" state="ready" {
  goal "Wait for a person without rewriting the loop state."
  completion { when "all-steps-exhausted" }
  loop "improve" {
    max-rounds 2
    until {
      gate "accept" type="human" {
        reviewer "person/example"
        question "Accept this round?"
      }
    }
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
        apply_source(&store, source, "gated-loop-source");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "gated-loop".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "gated-loop-run".into(),
            })
            .unwrap();
        let loop_subject = format!(
            "loop-run/{}/improve",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let mut waiting = false;
        for _ in 0..20 {
            reconciler.reconcile_once().unwrap();
            if store
                .gate_request_for_owner(&loop_subject)
                .unwrap()
                .is_some()
            {
                waiting = true;
                break;
            }
        }
        assert!(waiting, "the loop never asked for its gate");
        reconciler.reconcile_once().unwrap();
        let states = store
            .claims_for(&loop_subject, Some("loop.state"))
            .unwrap()
            .len();
        for _ in 0..30 {
            reconciler.reconcile_once().unwrap();
        }

        let after = store.claims_for(&loop_subject, Some("loop.state")).unwrap();
        assert_eq!(after.len(), states, "a waiting loop rewrote its state");
        let latest = after.last().unwrap();
        assert_eq!(
            latest
                .body
                .pointer("/fields/status")
                .and_then(Value::as_str),
            Some("running")
        );
        assert_eq!(
            latest
                .body
                .pointer("/fields/best_round")
                .and_then(Value::as_u64),
            Some(1),
            "the latest state keeps the round's best result"
        );
        assert_ne!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn a_render_receipt_is_recorded_once_on_the_node_that_rendered() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let receipt = |sha: &str| {
            BTreeMap::from([(
                "writes".into(),
                serde_json::json!([{"destination": "/work/example/.st3/boot.md", "mode": 420, "sha256": sha}]),
            )])
        };
        let generation = *reconciler.event_notify.borrow();
        reconciler
            .record_once("agent/node.worker", "render.applied", receipt("first"))
            .unwrap();
        reconciler
            .record_once("agent/node.worker", "render.applied", receipt("first"))
            .unwrap();
        assert_eq!(
            store
                .observations_for("agent/node.worker", "render.applied")
                .unwrap()
                .len(),
            1,
            "an unchanged receipt is not recorded again"
        );
        assert!(
            store
                .claims_for("agent/node.worker", Some("render.applied"))
                .unwrap()
                .is_empty(),
            "a render receipt stays on this node"
        );
        assert_eq!(*reconciler.event_notify.borrow(), generation + 1);
        reconciler
            .record_once("agent/node.worker", "render.applied", receipt("second"))
            .unwrap();
        let receipts = store
            .observations_for("agent/node.worker", "render.applied")
            .unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[1].body["fields"]["writes"][0]["sha256"], "second");
        assert_eq!(
            store
                .latest_observation("agent/node.worker", "render.applied")
                .unwrap()
                .unwrap()
                .id,
            receipts[1].id
        );
    }

    #[test]
    fn a_failed_loop_requests_attention_once() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
resource "result" { kind "custom.test.loop-result" }
mission "alert-exhaustion" state="ready" {
  goal "Fail with a visible bounded-loop fault."
  completion { when "all-steps-exhausted" }
  loop "review" {
    max-rounds 1
    until { gate "ready" { field "state" "resource/result" is "ready" } }
    round { completion { when "all-steps-exhausted" } }
    on-exhausted {
      fail
      attention "Automatic review failed" {
        reviewer "person/nathan"
        severity "error"
      }
    }
  }
}
"#;
        apply_source(&store, source, "alert-loop-exhaustion");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "alert-exhaustion".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "alert-exhaustion-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..20 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "failed"
        );
        let attention = store.attention_items(Some("person/nathan")).unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].title, "Automatic review failed");
        assert_eq!(attention[0].kind, "fault");
        assert!(attention[0].targets.contains(&run.subject));
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.attention_items(Some("person/nathan")).unwrap().len(),
            1
        );
    }

    #[test]
    #[should_panic(expected = "removed-loop-shape")]
    fn a_removed_best_of_n_loop_is_rejected_before_reconciliation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2
resource "candidate-1" { kind "custom.test.candidate" }
resource "candidate-2" { kind "custom.test.candidate" }
mission "best" state="ready" {
  goal "Select the best bounded candidate."
  completion { when "all-steps-exhausted" }
  loop "choose" {
    max-rounds 2
    metric "quality" direction="higher" {
      field "score" "resource/candidate-${candidate.index}"
    }
    candidates 2 max-parallel=2 { select metric="quality" }
    round {
      completion { when "all-steps-exhausted" }
      step "create" { agentless; goal "Create candidate ${candidate.index}." }
    }
  }
}
"#;
        apply_source(&store, source, "candidate-loop");
        for (candidate, score) in [(1, 0.25), (2, 0.75)] {
            store
                .append_claim(&ClaimInput {
                    subject: format!("resource/candidate-{candidate}"),
                    kind: "resource.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("kind".into(), Value::String("custom.test.candidate".into())),
                        ("score".into(), Value::from(score)),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("candidate-score-{candidate}")),
                })
                .unwrap();
        }
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "best".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "best-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let mut initial_candidates = 0;
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
            initial_candidates = (1..=3)
                .filter(|candidate| {
                    let key = format!("loop-candidate:{}:1:{candidate}", run.steps[0].subject);
                    let subject = store.mission_run_subject_for_idempotency_key(&key);
                    store.mission_run(&subject).unwrap().is_some()
                })
                .count();
            if initial_candidates > 0 {
                break;
            }
        }
        assert_eq!(initial_candidates, 2);
        for _ in 0..40 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
        let subject = format!(
            "loop-run/{}/choose",
            run.generation.strip_prefix("run-generation/").unwrap()
        );
        let state = store
            .latest_claim(&subject, Some("loop.state"))
            .unwrap()
            .unwrap();
        assert_eq!(
            state.body.pointer("/fields/winner").and_then(Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn an_open_queue_does_not_report_standing_between_ready_items() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

mission "queue" state="ready" {
  goal "Complete an ordered queue."
  queue "work" {
    agentless
    step "one" { goal "Complete the first item." }
    step "two" { goal "Complete the second item." }
  }
}
"#;
        apply_source(&store, source, "publish-queue");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "queue".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-queue".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        loop {
            reconciler.reconcile_once().unwrap();
            let view = store.mission_run(&run.id).unwrap().unwrap();
            let all_completed = view.steps.iter().all(|step| step.status == "completed");
            assert!(all_completed || view.status == "running");
            if all_completed {
                for _ in 0..4 {
                    reconciler.reconcile_once().unwrap();
                }
                break;
            }
        }

        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn a_strict_queue_starts_agentless_nested_containers_in_order() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

mission "queue-nested" state="ready" {
  completion { when "all-steps-exhausted" }
  goal "Run two nested jobs without overlap."
  agent "worker" { workspace "/tmp"; command "true" }
  queue "jobs" {
    step "first-job" {
      mission "first" {
        assigned-to "agent/${ST_MISSION_RUN}/worker"
        goal "Complete the first nested job."
        step "work" {
          goal "Complete first work."
          retry { attempts 2 }
        }
      }
    }
    step "second-job" {
      mission "second" {
        assigned-to "agent/${ST_MISSION_RUN}/worker"
        goal "Complete the second nested job."
        step "work" { goal "Complete second work." }
      }
    }
  }
}
"#;
        apply_source(&store, source, "publish-queue-nested");
        let published = store.index().unwrap();
        apply_source(&store, source, "publish-queue-nested");
        assert_eq!(
            store.index().unwrap(),
            published,
            "duplicate publication changed the graph"
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "queue-nested".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-queue-nested".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }

        let view = store.mission_run(&run.id).unwrap().unwrap();
        let step = |path: &str| view.steps.iter().find(|step| step.step == path).unwrap();
        let first = step("first-job");
        let first_work = step("first-job/first/work");
        let second = step("second-job");
        let second_work = step("second-job/second/work");
        assert_eq!(first.status, "working");
        assert!(first.agentless);
        assert_eq!(first_work.status, "ready");
        assert_eq!(second.status, "pending");
        assert_eq!(second_work.status, "pending");
        let unavailable = store
            .work_action(
                &first.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some(first_work.assigned_to.clone().unwrap()),
                    incarnation: Some("current".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-structural-parent".into(),
                },
            )
            .unwrap_err();
        assert_eq!(unavailable.code, "work-not-available");
        assert!(store.step_run(&first.subject).unwrap().is_some());

        let work = |key: &str, actor: String| crate::model::WorkRequest {
            actor: Some(actor),
            incarnation: Some("current".into()),
            summary: None,
            reason: None,
            evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        let first_actor = first_work.assigned_to.clone().unwrap();
        let first_subject = first_work.subject.clone();
        store
            .work_action(
                &first_subject,
                "claim",
                &work("claim-first-nested", first_actor.clone()),
            )
            .unwrap();
        store
            .work_action(
                &first_subject,
                "fail",
                &work("fail-first-nested", first_actor.clone()),
            )
            .unwrap();
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }

        let after_retry = store.mission_run(&run.id).unwrap().unwrap();
        let retried = after_retry
            .steps
            .iter()
            .find(|step| step.step == "first-job/first/work")
            .unwrap();
        assert_eq!(retried.status, "ready");
        assert_eq!(retried.attempt, 2);
        assert_eq!(
            after_retry
                .steps
                .iter()
                .find(|step| step.step == "second-job")
                .unwrap()
                .status,
            "pending"
        );

        // Reconstructing the reconciler is the daemon-restart boundary. The persisted parent
        // admission and retry attempt resume without duplicating or advancing the queue item.
        drop(reconciler);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        store
            .work_action(
                &first_subject,
                "claim",
                &work("claim-first-nested-retry", first_actor.clone()),
            )
            .unwrap();
        store
            .work_action(
                &first_subject,
                "complete",
                &work("complete-first-nested-retry", first_actor),
            )
            .unwrap();
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }

        let view = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(
            view.steps
                .iter()
                .find(|step| step.step == "first-job")
                .unwrap()
                .status,
            "completed"
        );
        let second = view
            .steps
            .iter()
            .find(|step| step.step == "second-job")
            .unwrap();
        let second_work = view
            .steps
            .iter()
            .find(|step| step.step == "second-job/second/work")
            .unwrap();
        assert_eq!(second.status, "working");
        assert_eq!(second_work.status, "ready");
        let second_actor = second_work.assigned_to.clone().unwrap();
        let second_subject = second_work.subject.clone();
        store
            .work_action(
                &second_subject,
                "claim",
                &work("claim-second-nested", second_actor.clone()),
            )
            .unwrap();
        store
            .work_action(
                &second_subject,
                "complete",
                &work("complete-second-nested", second_actor),
            )
            .unwrap();
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        let view = store.mission_run(&run.id).unwrap().unwrap();
        assert!(view.steps.iter().all(|step| step.status == "completed"));
        assert_eq!(view.status, "completed");
    }

    #[test]
    fn a_top_level_seat_outlives_a_zero_step_mission() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  agent "standing-agent" {
    command "sleep 60"
    restart "on-failure"
    exec "helper" { command "true" }
  }

  mission "finite-zero" state="ready" {
    goal "Complete without owning the durable seat."
  }
"#;
        apply_source(&store, source, "publish-standing-agent");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "finite-zero".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-standing-agent".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }

        let subjects = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .map(|subject| subject.subject)
            .collect::<BTreeSet<_>>();
        assert!(subjects.contains("agent/node.standing-agent"));
        assert!(subjects.contains("exec/node.standing-agent/helper"));
        let started = runtime
            .started_members
            .lock()
            .unwrap()
            .iter()
            .map(|member| member.runtime_id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(started.len(), 2);
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn a_run_recovers_from_drain_after_its_proposal_is_gone() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"
version 2
mission "recover-drain" state="ready" {
  goal "Keep one step available while a stale drain is repaired."
  step "wait" {
    assigned-to "agent/missing"
    goal "Wait for a worker."
  }
}
"#,
            "publish-recover-drain",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "recover-drain".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-recover-drain".into(),
            })
            .unwrap();
        store
            .set_mission_run_state(&run.id, "running", "revision-draining", None)
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(store.mission_run(&run.id).unwrap().unwrap().phase, "normal");
    }

    #[test]
    fn a_terminal_mission_stops_its_owned_runtimes() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "finite" state="ready" {
    goal "Complete and stop the generation assertions."
    completion { when "all-steps-exhausted" }
     agent "worker" { workspace "/tmp"; command "true"; restart "never" }
    step "finish" { agentless }
  }

"#;
        apply_source(&store, source, "publish-finite");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "finite".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-finite".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
        assert!(store.desired_subjects().unwrap().iter().any(|desired| {
            desired.subject == format!("agent/{}/worker", run.id)
                && desired.kind == "stop"
                && desired.owner_run.as_deref() == Some(run.subject.as_str())
        }));
    }

    #[test]
    fn eval_cleanup_stops_the_exact_top_level_seat_it_owns() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "eval/demo/seat" { workspace "/tmp"; command "true"; restart "never" }

mission "eval/demo" state="ready" timeout="1m" {
  goal "Finish while the eval owns a top-level seat."
  step "finish" { agentless }
}
"#;
        let key = "eval-owned-seat";
        let owner = store.mission_run_subject_for_idempotency_key(key);
        let mut intent = parse_intent(source, "node").unwrap();
        for desired in intent.subjects.values_mut() {
            desired.owner_run = Some(owner.clone());
        }
        let mission = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &mission.subject_tokens, "eval-owned-seat-apply")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "eval/demo".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("eval".into()),
                inputs: BTreeMap::new(),
                idempotency_key: key.into(),
            })
            .unwrap();
        assert_eq!(run.subject, owner);
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let runtime_id = runtime.started_members.lock().unwrap()[0]
            .runtime_id
            .clone();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("seat-one".into()),
        });
        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }

        let seat = "agent/eval/demo/seat";
        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().phase,
            "cleanup-completed"
        );
        let desired = store.desired_subjects().unwrap();
        assert!(
            desired
                .iter()
                .any(|subject| subject.subject == seat && subject.kind == "stop"),
            "cleanup stops the exact seat the eval owns"
        );
        assert!(
            desired
                .iter()
                .all(|subject| subject.subject != format!("agent/{}/eval/demo/seat", run.id)),
            "cleanup must not invent a run-scoped copy of the seat"
        );
        assert!(runtime.stops.lock().unwrap().contains(&runtime_id));

        runtime.ptys.lock().unwrap().clear();
        for _ in 0..6 {
            reconciler.reconcile_once().unwrap();
        }
        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, "completed");
        assert_eq!(run.phase, "terminal");
        assert_eq!(
            store
                .latest_claim(&run.subject, Some("eval.verdict"))
                .unwrap()
                .unwrap()
                .body
                .pointer("/fields/verdict")
                .and_then(Value::as_str),
            Some("pass")
        );
    }

    #[test]
    fn a_remote_reconciler_does_not_observe_a_local_stop_for_another_host() {
        let store = Arc::new(Store::open_memory("Silber").unwrap());
        let subject = "agent/fleet/probe";
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("host".into(), Value::String("Silber".into())),
                    ("runtime_id".into(), Value::String("fleet.probe".into())),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let stop = DesiredSubject {
            subject: subject.into(),
            kind: "stop".into(),
            desired: Value::Null,
            member: None,
            owner_run: None,
            owner_generation: None,
            owner_step: None,
        };
        let remote = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "hetz".into(),
            Arc::new(Notify::new()),
        );
        remote.reconcile_stop(&stop, Some(&HashMap::new())).unwrap();
        assert_eq!(
            store
                .claims_for(subject, Some("runtime.observed"))
                .unwrap()
                .len(),
            1
        );
        let owner = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "Silber".into(),
            Arc::new(Notify::new()),
        );
        owner.reconcile_stop(&stop, Some(&HashMap::new())).unwrap();
        assert_eq!(
            store.latest_actual_value(subject).unwrap().unwrap()["status"],
            "stopped"
        );
    }

    #[test]
    fn a_finally_stop_does_not_relaunch_a_mission_agent() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

mission "final-stop-no-relaunch" state="ready" {
  goal "Stop the mission agent exactly once."
  completion { when "all-steps-exhausted" }

  agent "worker" { workspace "/tmp"; command "true"; restart "never" }

  step "finish" { agentless }

  finally {
    step "stop-worker" {
      agentless
      stop "agent/${ST_MISSION_RUN}/worker"
    }
  }
}
"#;
        apply_source(&store, source, "publish-final-stop-no-relaunch");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "final-stop-no-relaunch".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-final-stop-no-relaunch".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..12 {
            reconciler.reconcile_once().unwrap();
        }

        assert_eq!(
            store.mission_run(&run.id).unwrap().unwrap().status,
            "completed"
        );
        assert_eq!(
            runtime.started_members.lock().unwrap().len(),
            1,
            "the final stop must not be overwritten by a fresh mission-level declaration"
        );
        let worker = format!("agent/{}/worker", run.id);
        assert!(store.desired_subjects().unwrap().iter().any(|desired| {
            desired.subject == worker
                && desired.kind == "stop"
                && desired.owner_run.as_deref() == Some(run.subject.as_str())
                && desired.owner_generation.as_deref() == Some(run.generation.as_str())
        }));
    }

    #[test]
    fn a_terminal_owner_stops_a_runtime_even_without_a_cleanup_declaration() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "direct-terminal" state="ready" {
    goal "Stop runtimes after any authoritative terminal transition."
    completion { when "all-steps-exhausted" }
    agent "worker" { workspace "/tmp"; command "true"; restart "never" }
    step "finish" { agentless }
  }

"#;
        apply_source(&store, source, "publish-direct-terminal");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "direct-terminal".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-direct-terminal".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == format!("agent/{}/worker", run.id))
            .and_then(|desired| desired.member)
            .unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("direct-terminal-incarnation".into()),
        });
        reconciler.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: format!("agent/{}/worker", run.id),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("stopped".into())),
                    (
                        "runtime_id".into(),
                        Value::String(member.runtime_id.clone()),
                    ),
                    ("terminal".into(), Value::Bool(true)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("stale-stopped-worker".into()),
            })
            .unwrap();

        assert!(
            store
                .set_mission_run_state(
                    &run.id,
                    "cancelled",
                    "terminal",
                    Some("cancelled by an older client"),
                )
                .unwrap()
        );
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .any(
                    |desired| desired.subject == format!("agent/{}/worker", run.id)
                        && desired.kind == "agent"
                )
        );

        *runtime.snapshot_error.lock().unwrap() = true;
        reconciler.reconcile_once().unwrap();
        assert!(runtime.stops.lock().unwrap().is_empty());
        *runtime.snapshot_error.lock().unwrap() = false;
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            runtime.stops.lock().unwrap().as_slice(),
            &[member.runtime_id]
        );
        let actual = store
            .latest_actual_value(&format!("agent/{}/worker", run.id))
            .unwrap()
            .unwrap();
        assert_eq!(
            actual_field(&actual, "status").and_then(Value::as_str),
            Some("running"),
            "a live incarnation must displace the stale stopped projection"
        );
    }

    #[test]
    fn a_failed_dependency_selects_terminal_cleanup_for_a_finite_mission() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "blocked" state="ready" {
    goal "Expose an unreachable explicit completion frontier."
    completion { when "all-steps-exhausted" }
    step "failed" { agentless }
    step "dependent" { agentless; depends-on { step "failed" completed } }
  }

"#;
        apply_source(&store, source, "publish-blocked");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "blocked".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-blocked".into(),
            })
            .unwrap();
        let failed = run.steps.iter().find(|step| step.step == "failed").unwrap();
        store
            .set_step_state(&failed.subject, "failed", Some("test failure"))
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, "failed");
        assert_eq!(run.phase, "terminal");
        assert_eq!(
            run.steps
                .iter()
                .find(|step| step.step == "dependent")
                .unwrap()
                .status,
            "cancelled"
        );
    }

    #[test]
    fn a_retried_step_reopens_its_failed_run_and_the_run_completes() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "takeover" state="ready" {
    goal "Take over one service."
    agent "watcher" { workspace "/tmp"; command "true"; restart "never" }
    step "prepare" { agentless }
    step "deploy-check" { agentless; depends-on { step "prepare" completed } }
    step "announce" { agentless; depends-on { step "deploy-check" completed } }
    finally { step "report" { agentless } }
  }

"#;
        apply_source(&store, source, "publish-takeover");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "takeover".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-takeover".into(),
            })
            .unwrap();
        let step = |run: &crate::model::MissionRunView, path: &str| {
            run.steps
                .iter()
                .find(|step| step.step == path)
                .unwrap()
                .clone()
        };
        store
            .set_step_state(&step(&run, "prepare").subject, "completed", None)
            .unwrap();
        store
            .set_step_state(
                &step(&run, "deploy-check").subject,
                "failed",
                Some("the deploy check failed"),
            )
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let settle = || {
            for _ in 0..20 {
                reconciler.reconcile_once().unwrap();
                if store.mission_run(&run.id).unwrap().unwrap().phase == "terminal" {
                    break;
                }
            }
            store.mission_run(&run.id).unwrap().unwrap()
        };
        let failed = settle();
        assert_eq!(failed.status, "failed");
        assert_eq!(step(&failed, "announce").status, "cancelled");
        assert_eq!(step(&failed, "report").status, "completed");
        // The watcher is the only member this mission declares.
        let watcher_starts = || runtime.started_members.lock().unwrap().len();
        assert_eq!(watcher_starts(), 1);

        let reopened = store
            .retry_failed_step(
                &step(&failed, "deploy-check").subject,
                "person/test",
                "the deploy check host is back",
                "retry-deploy-check",
            )
            .unwrap();
        assert_eq!(reopened.status, "running");

        let finished = settle();
        assert_eq!(finished.status, "completed");
        assert_eq!(finished.generation, reopened.generation);
        assert_eq!(step(&finished, "deploy-check").attempt, 2);
        assert_eq!(
            watcher_starts(),
            2,
            "the reopened run relaunches its mission agent"
        );
        assert!(
            finished.steps.iter().all(|step| step.status == "completed"),
            "{:?}",
            finished.steps
        );
    }

    #[test]
    fn one_agent_can_claim_only_one_independent_pool_step_and_a_claim_is_exclusive() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  agent "one" { workspace "/tmp"; command "true"; restart "never" }
  mission "pool" state="ready" {
    goal "Expose two steps to one explicit pool."
    available-to "agent/node.one"
    available-to "agent/node.missing"
    step "a" { }
    step "b" { }
  }

"#;
        apply_source(&store, source, "publish-pool");
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.one".into(),
                kind: "harness.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("incarnation-one".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("pool-agent-ready".into()),
            })
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "pool".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-pool".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.evaluate_mission_runs().unwrap();
        let work = store.work(Some("agent/node.one"), false).unwrap();
        assert_eq!(work.len(), 2);
        let claim = |step: &crate::model::StepRunView, key: &str| {
            store.work_action(
                &step.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.one".into()),
                    incarnation: Some("incarnation-one".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: key.into(),
                },
            )
        };
        claim(&work[0], "pool-claim-a").unwrap();
        let capacity = claim(&work[1], "pool-claim-b").unwrap_err();
        assert_eq!(capacity.code, "agent-capacity");
        let run = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(
            run.steps
                .iter()
                .filter(|step| step.claimant.as_deref() == Some("agent/node.one"))
                .count(),
            1
        );
        let error = store
            .work_action(
                &work[0].subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.missing".into()),
                    incarnation: Some("other".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "pool-lost-race".into(),
                },
            )
            .unwrap_err();
        assert!(
            matches!(
                error.code,
                "work-already-claimed"
                    | "invalid-work-transition"
                    | "work-not-eligible"
                    | "agent-not-active"
            ),
            "{}",
            error.code
        );
    }

    struct FakeResourceProvider;

    struct InvalidRepositoryProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ResourceProvider for InvalidRepositoryProvider {
        fn observe(
            &self,
            _request: ObservationRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::resource::ProviderObservation {
                    facts: serde_json::json!({"issues": "not-an-array"}),
                    cursor: Some("invalid-cursor".into()),
                    next_check_unix_ms: now_ms().saturating_add(60_000),
                })
            })
        }
    }

    #[tokio::test]
    async fn a_rejected_observation_degrades_and_pauses_its_revision() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
resource "repo" { kind "vcs.repository" }
observer "repo" {
  resource "resource/repo"
  provider "github.repository"
  locator "owner/repo"
  field "issues"
}"#,
            "rejected-observation",
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let (event_notify, mut event_changed) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(InvalidRepositoryProvider {
            calls: calls.clone(),
        }))
        .with_event_notify(event_notify);
        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .unwrap()
            .unwrap();
        let state = store.latest_actual_value("observer/repo").unwrap().unwrap();
        assert_eq!(state["state"], "degraded");
        assert!(state["reason"].as_str().unwrap().contains("issues"));
        assert!(
            store
                .claims_for("resource/repo", Some("resource.observed"))
                .unwrap()
                .is_empty()
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A refresh request still polls the paused observer, once.
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "observer/repo".into(),
                kind: "observer.refresh-requested".into(),
                actor: Some("person/test".into()),
                fields: BTreeMap::from([
                    ("revision".into(), Value::String(revision)),
                    ("attempt".into(), Value::String("refresh-1".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .pending_observer_refresh_attempt("observer/repo")
                .unwrap(),
            None
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    struct RateLimitedResourceProvider {
        calls: Arc<AtomicUsize>,
        retry_at_unix_ms: u128,
    }

    impl ResourceProvider for RateLimitedResourceProvider {
        fn observe(
            &self,
            _request: ObservationRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(crate::resource::ProviderRateLimit {
                    retry_at_unix_ms: self.retry_at_unix_ms,
                    unauthenticated: false,
                    status: 429,
                }))
            })
        }
    }

    #[tokio::test]
    async fn a_rate_limited_observer_waits_until_the_provider_reset() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
resource "repo" { kind "vcs.repository" }
observer "repo" {
  resource "resource/repo"
  provider "github.repository"
  locator "example/repo"
  field "issues"
}"#,
            "rate-limited-observer",
        );
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        let deadline_key = format!("observer/repo:{revision}");
        let reset = now_ms() + 180_000;
        let calls = Arc::new(AtomicUsize::new(0));
        let (event_notify, mut event_changed) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(RateLimitedResourceProvider {
            calls: calls.clone(),
            retry_at_unix_ms: reset,
        }))
        .with_event_notify(event_notify);
        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reconciler.observer_deadlines.lock().unwrap()[&deadline_key],
            reset
        );
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_observer_unreachable_for_an_hour_requests_attention_once() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("observer.sqlite");
        let store = Arc::new(Store::open(&database, "node").unwrap());
        apply_source(
            &store,
            r#"version 2
resource "repo" { kind "vcs.repository" }
observer "repo" {
  resource "resource/repo"
  provider "github.repository"
  locator "example/repo"
  field "issues"
}"#,
            "prolonged-unreachable-observer",
        );
        let revision = store
            .selected_desired_revision("observer/repo")
            .unwrap()
            .unwrap();
        let old = store
            .append_claim(&ClaimInput {
                subject: "observer/repo".into(),
                kind: "observer.state".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("state".into(), Value::String("unreachable".into())),
                    ("reason".into(), Value::String("old failure".into())),
                    ("revision".into(), Value::String(revision)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE claims SET accepted_at_unix_ms=?1 WHERE id=?2",
                rusqlite::params![(now_ms() - 3_600_001).to_string(), old.id],
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (event_notify, mut event_changed) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(RateLimitedResourceProvider {
            calls,
            retry_at_unix_ms: now_ms() + 180_000,
        }))
        .with_event_notify(event_notify);
        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .unwrap()
            .unwrap();
        let attention = store.attention_items(None).unwrap();
        assert_eq!(
            attention
                .iter()
                .filter(|item| item.title == "GitHub observer needs attention")
                .count(),
            1
        );
        assert_eq!(
            attention
                .iter()
                .find(|item| item.title == "GitHub observer needs attention")
                .unwrap()
                .person,
            "person/operator"
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store
                .attention_items(None)
                .unwrap()
                .iter()
                .filter(|item| item.title == "GitHub observer needs attention")
                .count(),
            1
        );
    }

    struct SharedDiscoveryProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ResourceProvider for SharedDiscoveryProvider {
        fn observe(
            &self,
            _request: ObservationRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::resource::ProviderObservation {
                    facts: serde_json::json!({"issues": [{"number": 7}]}),
                    cursor: Some("issue-seven".into()),
                    next_check_unix_ms: now_ms().saturating_add(60_000),
                })
            })
        }
    }

    #[tokio::test]
    async fn observers_on_one_resource_deliver_to_both_subscriptions() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  goal "Review a discovered issue."
  step "review" { agentless }
}"#,
            "shared-discovery-mission",
        );
        let revision = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let source = format!(
            r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "a" {{ resource "resource/repo"; provider "github.repository"; locator "example/repo"; field "issues" }}
observer "b" {{ resource "resource/repo"; provider "github.repository"; locator "example/repo"; field "issues" }}
subscription "a" {{
  observer "observer/a"
  on "issues"
  delivery "mission" {{ mission "review@{revision}"; resource "source"; workspace "/tmp/st3-review" }}
}}
subscription "b" {{
  observer "observer/b"
  on "issues"
  delivery "mission" {{ mission "review@{revision}"; resource "source"; workspace "/tmp/st3-review" }}
}}"#
        );
        apply_source(&store, &source, "shared-discovery-watch");
        store
            .record_resource_observation(
                "observer/a",
                &store
                    .selected_desired_revision("observer/a")
                    .unwrap()
                    .unwrap(),
                None,
                "resource/repo",
                Some("baseline"),
                &serde_json::json!({"issues": []}),
                now_ms() + 60_000,
                &[],
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(SharedDiscoveryProvider {
            calls: calls.clone(),
        }));
        reconciler
            .reconcile_resource_observers(&store.desired_subjects().unwrap())
            .unwrap();
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) == 2
                && reconciler.armed_observers.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        for subscription in ["subscription/a", "subscription/b"] {
            assert_eq!(
                store
                    .claims_for(subscription, Some("subscription.mission-requested"))
                    .unwrap()
                    .len(),
                1,
                "{subscription} did not receive issue 7"
            );
        }
    }

    struct BlockingResourceProvider {
        calls: Arc<AtomicUsize>,
        release: Arc<Notify>,
    }

    impl ResourceProvider for BlockingResourceProvider {
        fn observe(
            &self,
            _request: ObservationRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.release.notified().await;
                Ok(crate::resource::ProviderObservation {
                    facts: serde_json::json!({"state": "open"}),
                    cursor: Some("one".into()),
                    next_check_unix_ms: now_ms().saturating_add(60_000),
                })
            })
        }
    }

    #[tokio::test]
    async fn an_observer_has_only_one_provider_call_in_flight() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

resource "one" { kind "custom.example.state" }
observer "one" {
  resource "resource/one"
  provider "example.state"
  locator "one"
  field "state"
}
"#;
        apply_source(&store, source, "publish-one-observer");
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(BlockingResourceProvider {
            calls: calls.clone(),
            release: release.clone(),
        }));

        reconciler.reconcile_once().unwrap();
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.notify_waiters();
    }

    #[test]
    fn an_invalid_subscription_request_is_recorded_and_does_not_block_reconciliation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  completion { when "all-steps-exhausted" }
  goal "Review a discovered item."
  step "review" { agentless }
}"#,
            "subscription-request-mission",
        );
        let revision = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        apply_source(
            &store,
            &format!(
                r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "repo" {{
  resource "resource/repo"
  provider "github.repository"
  locator "example/repo"
  field "issues"
}}
subscription "reviews" {{
  observer "observer/repo"
  on "issues"
  delivery "mission" {{
    mission "review@{revision}"
    resource "source"
    workspace "/tmp/st3-review"
  }}
}}"#
            ),
            "subscription-request-failure",
        );
        let discovery = store
            .append_claim(&ClaimInput {
                subject: "resource/repo".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("kind".into(), Value::String("vcs.repository".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let bad = store
            .append_claim(&ClaimInput {
                subject: "subscription/reviews".into(),
                kind: "subscription.mission-requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("mission".into(), Value::String("mission/review".into())),
                    ("mission_revision".into(), Value::String(revision.clone())),
                    ("resource".into(), Value::String("resource/repo".into())),
                    ("resource_input".into(), Value::String("item".into())),
                    ("workspace".into(), Value::String("/tmp/st3-review".into())),
                    ("discovery".into(), Value::String(discovery.id.clone())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let good = store
            .append_claim(&ClaimInput {
                subject: "subscription/reviews".into(),
                kind: "subscription.mission-requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("mission".into(), Value::String("mission/review".into())),
                    ("mission_revision".into(), Value::String(revision)),
                    ("resource".into(), Value::String("resource/repo".into())),
                    ("resource_input".into(), Value::String("source".into())),
                    ("workspace".into(), Value::String("/tmp/st3-review".into())),
                    ("discovery".into(), Value::String(discovery.id)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let desired = store.desired_subjects().unwrap();
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        let failures = store
            .claims_for("subscription/reviews", Some("subscription.mission-failed"))
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].body["fields"]["request"], bad.id);
        assert_eq!(failures[0].body["fields"]["code"], "invalid-mission-inputs");
        let starts = store
            .claims_for("subscription/reviews", Some("subscription.mission-started"))
            .unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].body["fields"]["request"], good.id);
        assert!(
            store
                .attention_items(None)
                .unwrap()
                .iter()
                .any(|item| { item.kind == "fault" && item.targets == ["subscription/reviews"] })
        );
        let failure_attention = store
            .attention_items(None)
            .unwrap()
            .into_iter()
            .find(|item| item.kind == "fault" && item.targets == ["subscription/reviews"])
            .unwrap();
        assert!(
            failure_attention
                .subject
                .starts_with("attention/subscription-failure-")
        );
        let resolved = store
            .resolve_attention(
                &failure_attention.subject,
                &crate::model::AttentionResolveRequest {
                    outcome: "resolved".into(),
                    reason: Some("Corrected the subscription input".into()),
                    actor: "person/nathan".into(),
                    idempotency_key: "resolve-subscription-failure".into(),
                },
            )
            .unwrap();
        assert_eq!(resolved.status, "resolved");
        assert!(
            !store
                .attention_items(None)
                .unwrap()
                .iter()
                .any(|item| item.subject == failure_attention.subject)
        );
    }

    #[test]
    fn subscription_publish_rejects_an_undeclared_mission_resource_input() {
        let store = Store::open_memory("node").unwrap();
        apply_source(
            &store,
            r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  goal "Review."
  step "review" { agentless }
}"#,
            "subscription-publish-mission",
        );
        let revision = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let source = format!(
            r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "repo" {{ resource "resource/repo"; provider "github.repository"; locator "example/repo"; field "issues" }}
subscription "reviews" {{
  observer "observer/repo"
  on "issues"
  delivery "mission" {{ mission "review@{revision}"; resource "item"; workspace "/tmp/st3-review" }}
}}"#
        );
        let intent = parse_intent(&source, "node").unwrap();
        let plan = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("undeclared resource input"))
        );
        let error = store
            .apply(
                &intent,
                &plan.subject_tokens,
                "invalid-subscription-publish",
            )
            .unwrap_err();
        assert_eq!(error.code, "invalid-subscription-resource-input");
    }

    #[test]
    fn a_changed_subscription_starts_one_exact_resource_input_mission() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
mission "review" state="ready" {
  concurrent-runs max=1
  input "source" kind="resource"
  completion { when "all-steps-exhausted" }
  goal "Review one discovered item."
  step "review" { agentless }
}"#,
            "review-mission",
        );
        let revision = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let source = format!(
            r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "repo" {{ resource "resource/repo"; provider "github.repository"; locator "owner/repo"; field "pull_requests" }}
subscription "reviews" {{
  observer "observer/repo"
  on "pull_requests"
  delivery "mission" {{
    mission "review@{revision}"
    resource "source"
    workspace "/tmp/st3-review"
    requester "agent/fleet/repository/standing/owner"
  }}
}}"#
        );
        apply_source(&store, &source, "repository-watch");
        let desired = store.desired_subjects().unwrap();
        let subscription = desired
            .iter()
            .find(|item| item.kind == "subscription")
            .unwrap();
        let spec = crate::graph::subscription_spec(&subscription.desired).unwrap();
        let subscriptions = vec![(subscription.subject.clone(), spec)];
        let baseline = store
            .record_resource_observation(
                "observer/repo",
                &store
                    .selected_desired_revision("observer/repo")
                    .unwrap()
                    .unwrap(),
                None,
                "resource/repo",
                Some("one"),
                &serde_json::json!({"pull_requests": []}),
                now_ms() + 60_000,
                &subscriptions,
            )
            .unwrap();
        let baseline_claim = baseline
            .observation_claim
            .expect("the baseline creates a resource claim");
        let occupied = store
            .create_mission_run(&MissionRunRequest {
                mission: "review".into(),
                revision: Some(revision.clone()),
                workspace: "/tmp/st3-review/occupied".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::from([(
                    "source".into(),
                    format!("resource/repo@{baseline_claim}"),
                )]),
                idempotency_key: "occupied-review".into(),
            })
            .unwrap();
        let changed = store
            .record_resource_observation(
                "observer/repo",
                &store
                    .selected_desired_revision("observer/repo")
                    .unwrap()
                    .unwrap(),
                None,
                "resource/repo",
                Some("two"),
                &serde_json::json!({"pull_requests": [{"number": 7}]}),
                now_ms() + 60_000,
                &subscriptions,
            )
            .unwrap();
        let changed_claim = changed
            .observation_claim
            .expect("the changed observation creates a resource claim");
        let item_claim = store
            .claims_for("resource/repo/pull-request/7", Some("resource.observed"))
            .unwrap()
            .pop()
            .expect("the repository discovery creates one pull request resource")
            .id;
        assert_ne!(item_claim, changed_claim);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        assert!(
            store
                .claims_for("subscription/reviews", Some("subscription.mission-started"))
                .unwrap()
                .is_empty(),
            "capacity must leave the event pending"
        );
        let deferred = store
            .claims_for(
                "subscription/reviews",
                Some("subscription.mission-deferred"),
            )
            .unwrap();
        assert_eq!(deferred.len(), 1);
        assert!(
            deferred[0].body["fields"]["not_before_unix_ms"]
                .as_u64()
                .unwrap()
                > now_ms() as u64
        );
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        assert_eq!(
            store
                .claims_for(
                    "subscription/reviews",
                    Some("subscription.mission-deferred")
                )
                .unwrap()
                .len(),
            1
        );
        for _ in 0..5 {
            reconciler.evaluate_mission_runs().unwrap();
        }
        assert_eq!(
            store.mission_run(&occupied.id).unwrap().unwrap().status,
            "completed"
        );
        std::thread::sleep(Duration::from_millis(1_100));
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        let runs = store.active_mission_runs_for_mission("review").unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].revision, revision);
        assert_eq!(runs[0].requester, "agent/fleet/repository/standing/owner");
        assert_eq!(
            runs[0].inputs["source"].subject.as_deref(),
            Some("resource/repo/pull-request/7")
        );
        assert_eq!(runs[0].inputs["source"].claim_id, Some(item_claim));
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        assert_eq!(
            store
                .active_mission_runs_for_mission("review")
                .unwrap()
                .len(),
            1
        );
    }

    const REDELIVERY_REVIEW_SOURCE: &str = r#"version 2
mission "review" state="ready" {
  concurrent-runs max=10
  input "source" kind="resource"
  goal "Review one discovered item."
  step "review" { agentless }
}"#;

    /// Declare one repository observer at `locator` and one issue-triage subscription.
    fn watch_repository(
        store: &Store,
        locator: &str,
        review: &str,
        key: &str,
    ) -> Vec<(String, crate::model::SubscriptionSpec)> {
        apply_source(
            store,
            &format!(
                r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "repo" {{ resource "resource/repo"; provider "github.repository"; locator "{locator}"; field "issues" }}
subscription "triage" {{
  observer "observer/repo"
  on "issues"
  delivery "mission" {{
    mission "review@{review}"
    resource "source"
    workspace "/tmp/st3-triage"
  }}
}}"#
            ),
            key,
        );
        let desired = store.desired_subjects().unwrap();
        let subscription = desired
            .iter()
            .find(|item| item.subject == "subscription/triage")
            .unwrap();
        vec![(
            subscription.subject.clone(),
            crate::graph::subscription_spec(&subscription.desired).unwrap(),
        )]
    }

    fn record_issues(
        store: &Store,
        facts: &Value,
        subscriptions: &[(String, crate::model::SubscriptionSpec)],
    ) {
        store
            .record_resource_observation(
                "observer/repo",
                &store
                    .selected_desired_revision("observer/repo")
                    .unwrap()
                    .unwrap(),
                None,
                "resource/repo",
                None,
                facts,
                now_ms() + 60_000,
                subscriptions,
            )
            .unwrap();
    }

    /// List open issues the way the GitHub repository provider does and record the observation.
    fn observe_issues(
        store: &Store,
        locator: &str,
        numbers: impl IntoIterator<Item = u64>,
        subscriptions: &[(String, crate::model::SubscriptionSpec)],
    ) {
        let previous = store
            .latest_actual_value("resource/repo")
            .unwrap()
            .and_then(|actual| actual.get("facts").cloned());
        let issues = numbers
            .into_iter()
            .map(|number| {
                serde_json::json!({
                    "number": number,
                    "title": format!("Issue {number}"),
                    "html_url": format!("https://github.com/{locator}/issues/{number}"),
                })
            })
            .collect::<Vec<_>>();
        let facts = crate::resource::normalize_github_repository(
            previous.as_ref(),
            7,
            &[],
            &issues,
            &BTreeSet::from(["issues".into()]),
        )
        .unwrap();
        record_issues(store, &facts, subscriptions);
    }

    #[test]
    fn a_renamed_locator_and_a_first_complete_listing_deliver_nothing() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(&store, REDELIVERY_REVIEW_SOURCE, "review-mission");
        let review = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let subscriptions = watch_repository(&store, "acme/old", &review, "watch-old");
        // Facts from the first-page reader: it only ever saw the newest issues.
        let first_page = (145..=150)
            .map(|number| serde_json::json!({"number": number, "title": format!("Issue {number}")}))
            .collect::<Vec<_>>();
        record_issues(
            &store,
            &serde_json::json!({"issues": first_page}),
            &subscriptions,
        );
        let requests = || {
            store
                .claims_for(
                    "subscription/triage",
                    Some("subscription.mission-requested"),
                )
                .unwrap()
        };

        observe_issues(&store, "acme/old", 20..=150, &subscriptions);
        assert!(
            requests().is_empty(),
            "a first complete listing finds missed issues, not new ones"
        );

        let subscriptions = watch_repository(&store, "acme/new", &review, "watch-renamed");
        observe_issues(&store, "acme/new", 20..=150, &subscriptions);
        assert!(
            requests().is_empty(),
            "a renamed locator keeps every item identity"
        );

        observe_issues(&store, "acme/new", (20..=150).chain([151]), &subscriptions);
        let requests = requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].body["fields"]["resource"],
            "resource/repo/issue/151"
        );
    }

    #[test]
    fn one_observation_holds_excess_deliveries_for_a_person() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(&store, REDELIVERY_REVIEW_SOURCE, "review-mission");
        let review = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        let subscriptions = watch_repository(&store, "acme/repo", &review, "watch");
        observe_issues(&store, "acme/repo", [1], &subscriptions);
        observe_issues(&store, "acme/repo", 1..=8, &subscriptions);
        let statuses = || {
            store
                .subscription_requests("subscription/triage")
                .unwrap()
                .into_iter()
                .map(|request| (request.request, request.status))
                .collect::<Vec<_>>()
        };
        let requested = statuses();
        assert_eq!(requested.len(), 7);
        let held = requested
            .iter()
            .filter(|(_, status)| status == "held")
            .map(|(request, _)| request.clone())
            .collect::<Vec<_>>();
        assert_eq!(held.len(), 7 - crate::store::MAX_OBSERVATION_DELIVERIES);

        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let desired = store.desired_subjects().unwrap();
        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        let runs = || {
            store
                .active_mission_runs_for_mission("review")
                .unwrap()
                .len()
        };
        assert_eq!(runs(), crate::store::MAX_OBSERVATION_DELIVERIES);
        let attention = store.attention_items(Some("person/operator")).unwrap();
        assert_eq!(
            attention
                .iter()
                .filter(|item| item.title == "A subscription is holding mission requests")
                .count(),
            1
        );

        let decide = |request: &str, decision: &str, actor: &str, key: &str| {
            store.decide_subscription_request(
                request,
                decision,
                &crate::model::SubscriptionRequestDecision {
                    actor: actor.into(),
                    reason: "a person reviewed the held request".into(),
                    idempotency_key: key.into(),
                },
            )
        };
        let agent = decide(&held[0], "release", "agent/node.triage", "agent-release").unwrap_err();
        assert_eq!(agent.code, "subscription-request-person-only");
        assert_eq!(
            decide(&held[0], "release", "person/operator", "release")
                .unwrap()
                .status,
            "pending"
        );
        assert_eq!(
            decide(&held[1], "cancel", "person/operator", "cancel")
                .unwrap()
                .status,
            "cancelled"
        );
        assert_eq!(
            decide(&held[1], "cancel", "person/operator", "cancel")
                .unwrap()
                .status,
            "cancelled",
            "an exact retry returns the recorded decision"
        );
        let closed = decide(&held[1], "release", "person/operator", "late-release").unwrap_err();
        assert_eq!(closed.code, "subscription-request-not-open");

        reconciler
            .reconcile_subscription_missions(&desired)
            .unwrap();
        assert_eq!(runs(), crate::store::MAX_OBSERVATION_DELIVERIES + 1);
        let final_statuses = statuses();
        assert_eq!(
            final_statuses
                .iter()
                .filter(|(_, status)| status == "started")
                .count(),
            crate::store::MAX_OBSERVATION_DELIVERIES + 1
        );
        assert_eq!(
            final_statuses
                .iter()
                .filter(|(_, status)| status == "cancelled")
                .count(),
            1
        );
    }

    const INTAKE_REVIEW_SOURCE: &str = r#"version 2
mission "review" state="ready" {
  input "source" kind="resource"
  goal "Review one discovered item."
  step "review" { agentless }
}"#;

    /// A standing mission whose seat keeps the run open while it declares `intake`.
    fn standing_intake_source(intake: &str) -> String {
        format!(
            r#"version 2
resource "repo" {{ kind "vcs.repository" }}
mission "standing" state="ready" {{
  goal "Watch one repository."
  agent "seat" {{ workspace "/tmp"; command "true"; restart "never" }}
  step "watch" {{ assigned-to "agent/${{ST_MISSION_RUN}}/seat"; goal "Keep watching." }}
{intake}
}}"#
        )
    }

    fn standing_intake(review_revision: &str) -> String {
        format!(
            r#"  observer "repo" {{ resource "resource/repo"; provider "github.repository"; locator "owner/repo"; field "pull_requests" }}
  subscription "reviews" {{
    observer "observer/repo"
    on "pull_requests"
    delivery "mission" {{
      mission "review@{review_revision}"
      resource "source"
      workspace "/tmp/st3-review"
    }}
  }}"#
        )
    }

    fn request_review(store: &Store, subscription: &str, review_revision: &str, key: &str) {
        store
            .append_claim(&ClaimInput {
                subject: subscription.into(),
                kind: "subscription.mission-requested".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("mission".into(), Value::String("mission/review".into())),
                    (
                        "mission_revision".into(),
                        Value::String(review_revision.into()),
                    ),
                    (
                        "resource".into(),
                        Value::String("resource/repo/pull-request/7".into()),
                    ),
                    ("resource_input".into(), Value::String("source".into())),
                    ("workspace".into(), Value::String("/tmp/st3-review".into())),
                    ("discovery".into(), Value::String("discovery-claim".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(key.into()),
            })
            .unwrap();
    }

    #[test]
    fn a_revision_keeps_a_still_declared_member_and_its_claim() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "keep" state="ready" {
    goal "Keep the seat through a revision."
    agent "worker" { workspace "/tmp"; command "true"; restart "never" }
    step "work" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Do the work." }
    step "other" { assigned-to "agent/${ST_MISSION_RUN}/worker"; goal "Use the first goal." }
  }

"#;
        apply_source(&store, source, "publish-keep");
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "keep".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-keep".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            if !runtime.started_members.lock().unwrap().is_empty() {
                break;
            }
            reconciler.reconcile_once().unwrap();
        }
        let worker = format!("agent/{}/worker", run.id);
        let member = runtime.started_members.lock().unwrap()[0].clone();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("keep-incarnation".into()),
        });
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let work = |run: &crate::model::MissionRunView| {
            run.steps
                .iter()
                .find(|step| step.step == "work")
                .unwrap()
                .clone()
        };
        let old = work(&store.mission_run(&run.id).unwrap().unwrap());
        assert_eq!(old.status, "ready");
        store
            .work_action(
                &old.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some(worker.clone()),
                    incarnation: Some("keep-incarnation".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "keep-claim".into(),
                },
            )
            .unwrap();
        apply_source(
            &store,
            &source.replace("Use the first goal.", "Use the second goal."),
            "publish-keep-two",
        );
        let revised = store.mission_spec("keep", None).unwrap().unwrap();
        let adopted = store
            .adopt_mission_revision(
                &run.id,
                &revised,
                "person/test",
                "the other step needs a second goal",
                "keep-revision",
            )
            .unwrap();

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }

        assert!(
            runtime.stops.lock().unwrap().is_empty(),
            "a revision must not stop a member that its successor still declares"
        );
        assert_eq!(runtime.started_members.lock().unwrap().len(), 1);
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == worker)
            .unwrap();
        assert_eq!(desired.kind, "agent");
        assert_eq!(
            desired.owner_generation.as_deref(),
            Some(adopted.generation.as_str())
        );
        let carried = work(&store.mission_run(&run.id).unwrap().unwrap());
        assert_ne!(carried.subject, old.subject);
        assert_eq!(
            (
                carried.status.as_str(),
                carried.claimant.as_deref(),
                carried.claim_incarnation.as_deref()
            ),
            ("claimed", Some(worker.as_str()), Some("keep-incarnation"))
        );
    }

    #[test]
    fn a_revision_that_removes_intake_retires_it_and_cancels_unstarted_requests() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(&store, INTAKE_REVIEW_SOURCE, "review-mission");
        let review = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        apply_source(
            &store,
            &standing_intake_source(&standing_intake(&review)),
            "standing-with-intake",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "standing".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-standing".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let observer = format!("observer/{}/repo", run.id);
        let subscription = format!("subscription/{}/reviews", run.id);
        let desired = |subject: &str| {
            store
                .desired_subjects()
                .unwrap()
                .into_iter()
                .find(|desired| desired.subject == subject)
                .unwrap_or_else(|| panic!("`{subject}` is desired"))
        };
        assert!(!intake_is_stopped(&desired(&subscription), "node"));
        apply_source(
            &store,
            &standing_intake_source(""),
            "standing-without-intake",
        );
        let revised = store.mission_spec("standing", None).unwrap().unwrap();
        store
            .adopt_mission_revision(
                &run.id,
                &revised,
                "person/test",
                "stop the old intake",
                "drop-intake",
            )
            .unwrap();
        request_review(&store, &subscription, &review, "pending-review");

        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }

        assert!(
            store
                .claims_for(&subscription, Some("subscription.mission-started"))
                .unwrap()
                .is_empty(),
            "a removed subscription must not start its pending request"
        );
        assert!(
            store
                .active_mission_runs_for_mission("review")
                .unwrap()
                .is_empty()
        );
        let cancelled = store
            .claims_for(
                &subscription,
                Some("subscription.mission-request-cancelled"),
            )
            .unwrap();
        assert_eq!(cancelled.len(), 1);
        assert!(intake_is_stopped(&desired(&subscription), "node"));
        assert!(intake_is_stopped(&desired(&observer), "node"));
        assert_eq!(
            store
                .latest_actual_value(&observer)
                .unwrap()
                .and_then(|actual| actual.get("state").cloned()),
            Some(Value::String("stopped".into()))
        );

        request_review(&store, &subscription, &review, "late-review");
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store
                .claims_for(
                    &subscription,
                    Some("subscription.mission-request-cancelled")
                )
                .unwrap()
                .len(),
            2,
            "a request recorded after the stop is closed too"
        );
    }

    #[test]
    fn a_terminal_owner_or_another_host_starts_no_subscription_run() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(&store, INTAKE_REVIEW_SOURCE, "review-mission");
        let review = store
            .mission_spec("review", None)
            .unwrap()
            .unwrap()
            .revision;
        apply_source(
            &store,
            &standing_intake_source(&standing_intake(&review)),
            "standing-with-intake",
        );
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "standing".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "run-standing".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..3 {
            reconciler.reconcile_once().unwrap();
        }
        let owned = format!("subscription/{}/reviews", run.id);
        assert!(
            store
                .set_mission_run_state(&run.id, "completed", "terminal", None)
                .unwrap()
        );
        request_review(&store, &owned, &review, "after-completion");
        reconciler.reconcile_once().unwrap();
        assert!(
            store
                .claims_for(&owned, Some("subscription.mission-started"))
                .unwrap()
                .is_empty(),
            "a completed owner's subscription must not start work"
        );

        apply_source(
            &store,
            &format!(
                "version 2\nresource \"repo\" {{ kind \"vcs.repository\" }}\n{}",
                standing_intake(&review)
            ),
            "top-level-intake",
        );
        request_review(&store, "subscription/reviews", &review, "foreign-review");
        let peer = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "other".into(),
            Arc::new(Notify::new()),
        );
        peer.reconcile_subscription_missions(&store.desired_subjects().unwrap())
            .unwrap();
        assert!(
            store
                .claims_for("subscription/reviews", Some("subscription.mission-started"))
                .unwrap()
                .is_empty(),
            "only the declaring host starts subscription runs"
        );
    }

    #[test]
    fn a_resource_input_gate_reads_its_exact_start_claim() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  resource "source" { kind "custom.st3.document-source" }
  mission "resource-input" state="ready" {
    input "source" kind="resource"
    goal "Check the start snapshot."
    completion { when "all-steps-exhausted" }
    step "check" {
      agentless
      gate "the start snapshot is ready" { field "state" "${input.source}" is "ready" }
    }
  }

"#;
        apply_source(&store, source, "publish-resource-input");
        let first = store
            .append_claim(&ClaimInput {
                subject: "resource/source".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), Value::String("ready".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("resource-input-ready".into()),
            })
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "resource-input".into(),
                revision: None,
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::from([("source".into(), "resource/source".into())]),
                idempotency_key: "resource-input-run".into(),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "resource/source".into(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([("state".into(), Value::String("changed".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("resource-input-changed".into()),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        for _ in 0..5 {
            reconciler.reconcile_once().unwrap();
        }
        let completed = store.mission_run(&run.id).unwrap().unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.inputs["source"].claim_id, Some(first.id));
    }

    #[test]
    fn one_mission_run_rejects_a_runtime_id_in_two_steps_without_holding_later_runs() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "collision" state="ready" {
    goal "Reject two owners for one runtime."
    step "one" { agentless;  exec "same" { command "true"; restart "never" }  }
    step "two" { agentless;  exec "same" { command "true"; restart "never" }  }
  }

  mission "later" state="ready" {
    goal "Complete beside a faulted run."
    completion { when "all-steps-exhausted" }
    step "one" { }
  }

"#;
        apply_source(&store, source, "publish-collision");
        let request = |mission: &str| MissionRunRequest {
            mission: mission.into(),
            revision: None,
            workspace: ".".into(),
            requester: None,
            mode: None,
            inputs: BTreeMap::new(),
            idempotency_key: format!("{mission}-run"),
        };
        let collision = store.create_mission_run(&request("collision")).unwrap();
        // The faulted run is older, so it is evaluated first in every pass.
        std::thread::sleep(Duration::from_millis(2));
        let later = store.create_mission_run(&request("later")).unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .tolerating_faults();
        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }
        // The second step to declare the runtime faults; the run and its first step do not.
        let faults = store
            .mission_run(&collision.id)
            .unwrap()
            .unwrap()
            .steps
            .iter()
            .filter_map(|step| store.reconcile_fault(&step.subject, "step").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(
            faults[0].contains("more than one mission or step"),
            "{faults:?}"
        );
        assert_eq!(
            store
                .reconcile_fault(&collision.subject, "mission-run")
                .unwrap(),
            None
        );
        assert_eq!(
            store.mission_run(&later.id).unwrap().unwrap().status,
            "completed"
        );
        assert!(
            store
                .reconcile_fault("daemon/node", "stage/missions")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_step_stop_does_not_collide_with_its_mission_runtime() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "restart" state="ready" {
    goal "Restart one mission agent."
    completion { when "all-steps-exhausted" }
    agent "worker" { workspace "."; command "true"; restart "always" }
    step "restart-worker" {
      agentless
      stop "agent/${ST_MISSION_RUN}/worker"
    }
  }

"#;
        apply_source(&store, source, "publish-restart");
        store
            .create_mission_run(&MissionRunRequest {
                mission: "restart".into(),
                revision: None,
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "restart-run".into(),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        for _ in 0..8 {
            reconciler.reconcile_once().unwrap();
        }

        assert!(!runtime.starts.lock().unwrap().is_empty());
    }

    impl ResourceProvider for FakeResourceProvider {
        fn observe(
            &self,
            _request: ObservationRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::resource::ProviderObservation>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async {
                Ok(crate::resource::ProviderObservation {
                    facts: serde_json::json!({"state": "open"}),
                    cursor: Some("fake-one".into()),
                    next_check_unix_ms: now_ms().saturating_add(60_000),
                })
            })
        }
    }

    #[tokio::test]
    async fn an_observer_without_a_subscription_still_observes_its_resource() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  resource "standalone" { kind "custom.example.state" }
  observer "standalone" {
    resource "resource/standalone"
    provider "example.state"
    locator "standalone"
    field "state"
  }

"#;
        apply_source(&store, source, "publish-standalone-observer");
        let (event_notify, mut event_changed) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(FakeResourceProvider))
        .with_event_notify(event_notify.clone());

        reconciler.reconcile_once().unwrap();
        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .expect("the standalone observer did not finish")
            .expect("the event sender closed");

        assert_eq!(
            store
                .latest_actual_value("resource/standalone")
                .unwrap()
                .unwrap()["facts"]["state"],
            "open"
        );
        assert_eq!(
            store
                .latest_actual_value("observer/standalone")
                .unwrap()
                .unwrap()["status"],
            "healthy"
        );
    }

    #[tokio::test]
    async fn a_new_reconciler_record_wakes_event_waiters() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let (event_notify, mut event_changed) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_event_notify(event_notify.clone());
        reconciler
            .record_once(
                "daemon/node",
                "daemon.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("code".into(), Value::String("test".into())),
                    ("status".into(), Value::String("warning".into())),
                    ("reason".into(), Value::String("test record".into())),
                ]),
            )
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), event_changed.changed())
            .await
            .expect("the reconciler record did not wake the event waiter")
            .expect("the event sender closed");
    }

    #[tokio::test]
    async fn a_resource_observer_uses_one_shot_provider_work() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  agent "target" { workspace "/tmp"; command "true"; restart "never" }
  resource "github/acme/demo/pull/1" { kind "vcs.pull-request" }
  observer "github/acme/demo/pull/1" {
    resource "resource/github/acme/demo/pull/1"
    provider "github.pull-request"
    locator "acme/demo#1"
    field "state"
  }
  subscription "watch" {
    observer "observer/github/acme/demo/pull/1"
    to "agent/node.target"
    on "state"
    delivery "message"
  }

"#;
        apply_source(&store, source, "publish-fake-observer");
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_resource_provider(Arc::new(FakeResourceProvider));
        reconciler.reconcile_once().unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let facts = store
            .latest_actual_value("resource/github/acme/demo/pull/1")
            .unwrap()
            .unwrap();
        assert_eq!(facts["facts"]["state"], "open");
        assert!(store.messages(None, true).unwrap().is_empty());
        assert_eq!(
            store
                .latest_actual_value("observer/github/acme/demo/pull/1")
                .unwrap()
                .unwrap()["status"],
            "healthy"
        );
    }

    fn claude_seat_pty(seat: &str, status: &str, incarnation: &str) -> RuntimeObservation {
        RuntimeObservation {
            runtime_id: format!("node.{seat}"),
            terminal: true,
            status: status.into(),
            exit_code: (status == "exited").then_some(143),
            incarnation_id: Some(incarnation.into()),
        }
    }

    /// Three Claude seats start at once and two stop at the workspace trust prompt, as on
    /// 2026-09-27. Each blocked seat is reported not ready with the reason and replaced without
    /// anyone typing into its terminal, even under `restart "never"`. The seat that started
    /// normally is left alone.
    #[test]
    fn claude_seats_at_the_trust_prompt_are_reported_and_replaced_without_terminal_input() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().display().to_string();
        let source = format!(
            "version 2\n\
             agent \"seat-a\" {{ workspace {workspace:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n\
             agent \"seat-b\" {{ workspace {workspace:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n\
             agent \"seat-c\" {{ workspace {workspace:?}; restart \"never\"; harness \"claude\" {{ prompt \"Work.\" }} }}\n"
        );
        apply_source(&store, &source, "claude-trust-prompt");
        let runtime = Arc::new(FakeRuntime::default());
        *runtime.screen.lock().unwrap() = "╭─ Claude Code ─╮\n> ".into();
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.starts.lock().unwrap().len(), 3);

        *runtime.ptys.lock().unwrap() = vec![
            claude_seat_pty("seat-a", "running", "a-one"),
            claude_seat_pty("seat-b", "running", "b-one"),
            claude_seat_pty("seat-c", "running", "c-one"),
        ];
        runtime.screens.lock().unwrap().extend([
            ("node.seat-b".to_owned(), CLAUDE_TRUST_SCREEN.to_owned()),
            ("node.seat-c".to_owned(), CLAUDE_TRUST_SCREEN.to_owned()),
        ]);
        reconciler.reconcile_once().unwrap();

        for seat in ["agent/node.seat-b", "agent/node.seat-c"] {
            let harness = store.current_harness(seat).unwrap().unwrap();
            assert_eq!(
                harness.reason.as_deref(),
                Some("providerTrustPrompt"),
                "{seat}"
            );
            assert!(!harness.is_ready(), "{seat}");
        }
        assert!(
            store
                .claims_for("agent/node.seat-a", Some("harness.diagnostic"))
                .unwrap()
                .is_empty()
        );
        let mut stops = runtime.stops.lock().unwrap().clone();
        stops.sort();
        assert_eq!(stops, ["node.seat-b", "node.seat-c"]);

        // A pass while the stops are in flight repeats neither the report nor the stop.
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store
                .claims_for("agent/node.seat-b", Some("harness.diagnostic"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(runtime.stops.lock().unwrap().len(), 2);

        *runtime.ptys.lock().unwrap() = vec![
            claude_seat_pty("seat-a", "running", "a-one"),
            claude_seat_pty("seat-b", "exited", "b-one"),
            claude_seat_pty("seat-c", "exited", "c-one"),
        ];
        reconciler.reconcile_once().unwrap();
        let mut replaced = runtime.starts.lock().unwrap()[3..].to_vec();
        replaced.sort();
        assert_eq!(replaced, ["node.seat-b", "node.seat-c"]);

        runtime.screens.lock().unwrap().clear();
        *runtime.ptys.lock().unwrap() = vec![
            claude_seat_pty("seat-a", "running", "a-one"),
            claude_seat_pty("seat-b", "running", "b-two"),
            claude_seat_pty("seat-c", "running", "c-two"),
        ];
        reconciler.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.seat-b".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/node.seat-b".into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("incarnation_id".into(), Value::String("b-two".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(
            store
                .current_harness("agent/node.seat-b")
                .unwrap()
                .unwrap()
                .is_ready()
        );
        assert_eq!(runtime.stops.lock().unwrap().len(), 2);
        assert!(
            runtime.keys.lock().unwrap().is_empty(),
            "recovery must never type into a terminal"
        );
    }

    /// A seat whose screen shows the detector's own source, as a builder's grep output did on
    /// 2026-09-27, stays authenticated. A seat that shows Claude's login prompt is fenced, and its
    /// diagnostic records the exact screen line that matched.
    #[test]
    fn a_claude_login_prompt_line_fences_the_seat_and_records_the_matched_line() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().display().to_string();
        let source = format!(
            "version 2\n\
             agent \"seat-a\" {{ workspace {workspace:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n\
             agent \"seat-b\" {{ workspace {workspace:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n"
        );
        apply_source(&store, &source, "claude-login-line");
        let runtime = Arc::new(FakeRuntime::default());
        *runtime.screen.lock().unwrap() = "╭─ Claude Code ─╮\n> ".into();
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        *runtime.ptys.lock().unwrap() = vec![
            claude_seat_pty("seat-a", "running", "a-one"),
            claude_seat_pty("seat-b", "running", "b-one"),
        ];
        runtime.screens.lock().unwrap().extend([
            (
                "node.seat-a".to_owned(),
                concat!(
                    "● Bash(grep -n 'Login expired' crates/st3/src/reconcile.rs)\n",
                    "  ⎿  37:    screen.contains(\"Login expired · Please run /login\")\n",
                )
                .to_owned(),
            ),
            (
                "node.seat-b".to_owned(),
                "> Work.\n\n● Login expired · Please run /login\n".to_owned(),
            ),
        ]);
        reconciler.reconcile_once().unwrap();

        assert!(
            store
                .claims_for("agent/node.seat-a", Some("harness.diagnostic"))
                .unwrap()
                .is_empty()
        );
        let diagnostics = store
            .claims_for("agent/node.seat-b", Some("harness.diagnostic"))
            .unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].body.pointer("/fields/code"),
            Some(&Value::String("provider-auth-expired".into()))
        );
        assert_eq!(
            diagnostics[0].body.pointer("/fields/matched_line"),
            Some(&Value::String("● Login expired · Please run /login".into()))
        );
        let harness = store.current_harness("agent/node.seat-b").unwrap().unwrap();
        assert_eq!(harness.reason.as_deref(), Some("providerAuth"));
        let attention = store.attention_items(Some("person/nathan")).unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].targets, ["agent/node.seat-b"]);
    }

    #[test]
    fn a_claude_login_fence_lifts_when_the_prompt_leaves_the_screen() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"seat\" {{ workspace {:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "claude-login-lift");
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        *runtime.ptys.lock().unwrap() = vec![claude_seat_pty("seat", "running", "seat-one")];
        let show = |screen: &str| {
            runtime
                .screens
                .lock()
                .unwrap()
                .insert("node.seat".into(), screen.into());
        };
        let fenced = || {
            store
                .current_harness("agent/node.seat")
                .unwrap()
                .is_some_and(|harness| harness.reason.as_deref() == Some("providerAuth"))
        };

        show("> Work.\n\n● Login expired · Please run /login\n");
        reconciler.reconcile_once().unwrap();
        assert!(fenced());
        assert_eq!(
            store.attention_items(Some("person/nathan")).unwrap().len(),
            1
        );

        // The prompt is gone, so the same incarnation takes work again.
        show("> Work.\n\n● Done.\n");
        reconciler.reconcile_once().unwrap();
        assert!(!fenced());
        assert!(
            store
                .attention_items(Some("person/nathan"))
                .unwrap()
                .is_empty()
        );
        reconciler.reconcile_once().unwrap();
        let codes = store
            .claims_for("agent/node.seat", Some("harness.diagnostic"))
            .unwrap()
            .iter()
            .filter_map(|claim| {
                claim
                    .body
                    .pointer("/fields/code")?
                    .as_str()
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        assert_eq!(codes, ["provider-auth-expired", "provider-auth-restored"]);

        // The prompt returns: the incarnation is fenced again, with a new request.
        show("> Work.\n\n● Login expired · Please run /login\n");
        reconciler.reconcile_once().unwrap();
        assert!(fenced());
        assert_eq!(
            store.attention_items(Some("person/nathan")).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_claude_seat_that_keeps_reaching_the_trust_prompt_goes_to_a_person() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"seat\" {{ workspace {:?}; harness \"claude\" {{ prompt \"Work.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "claude-trust-prompt-repeated");
        let runtime = Arc::new(FakeRuntime::default());
        *runtime.screen.lock().unwrap() = CLAUDE_TRUST_SCREEN.into();
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();

        for attempt in 1..=CLAUDE_TRUST_RECOVERY_ATTEMPTS + 1 {
            let incarnation = format!("seat-{attempt}");
            *runtime.ptys.lock().unwrap() = vec![claude_seat_pty("seat", "running", &incarnation)];
            reconciler.reconcile_once().unwrap();
            if attempt <= CLAUDE_TRUST_RECOVERY_ATTEMPTS {
                *runtime.ptys.lock().unwrap() =
                    vec![claude_seat_pty("seat", "exited", &incarnation)];
                reconciler.reconcile_once().unwrap();
            }
        }

        assert_eq!(
            runtime.stops.lock().unwrap().len(),
            CLAUDE_TRUST_RECOVERY_ATTEMPTS
        );
        let harness = store.current_harness("agent/node.seat").unwrap().unwrap();
        assert_eq!(harness.reason.as_deref(), Some("providerTrustPrompt"));
        let attention = store.attention_items(Some("person/operator")).unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(
            attention[0].title,
            "Claude keeps stopping at its workspace trust prompt"
        );
        assert_eq!(attention[0].targets, ["agent/node.seat"]);
    }

    fn harness_claim(subject: &str, state: &str, incarnation: &str) -> ClaimInput {
        ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String(state.into())),
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("{subject}:{state}:{incarnation}")),
        }
    }

    #[test]
    fn a_daemon_restart_adopts_running_and_starting_seats_and_restarts_one_that_vanished() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("state.sqlite3");
        let workspace = tempfile::tempdir().unwrap();
        let source = ["running", "starting", "vanished"]
            .iter()
            .fold(String::from("version 2\n"), |source, name| {
                format!(
                    "{source}agent {name:?} {{ workspace {:?}; harness \"codex\" {{ prompt \"Wait.\" }} }}\n",
                    workspace.path().display().to_string()
                )
            });
        let pty = |name: &str| RuntimeObservation {
            runtime_id: format!("node.{name}"),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some(format!("{name}-one")),
        };
        let latest_runtime = |store: &Store, name: &str| {
            let actual = store
                .latest_actual_value(&format!("agent/node.{name}"))
                .unwrap()
                .unwrap();
            let fields = actual.get("fields").unwrap_or(&actual).clone();
            (
                fields["status"].as_str().unwrap_or_default().to_owned(),
                fields["incarnation_id"].as_str().map(str::to_owned),
            )
        };

        // Before the deploy: all three seats launched, and only one driver has reported ready.
        {
            let store = Arc::new(Store::open(&database, "node").unwrap());
            apply_source(&store, &source, "restart-seats");
            let runtime = Arc::new(FakeRuntime::default());
            let reconciler = Reconciler::new(
                store.clone(),
                runtime.clone(),
                "node".into(),
                Arc::new(Notify::new()),
            );
            reconciler.reconcile_once().unwrap();
            assert_eq!(runtime.starts.lock().unwrap().len(), 3);
            runtime
                .ptys
                .lock()
                .unwrap()
                .extend(["running", "starting", "vanished"].map(pty));
            reconciler.reconcile_once().unwrap();
            store
                .append_claim(&harness_claim("agent/node.running", "ready", "running-one"))
                .unwrap();
            for name in ["starting", "vanished"] {
                store
                    .append_claim(&harness_claim(
                        &format!("agent/node.{name}"),
                        "starting",
                        &format!("{name}-one"),
                    ))
                    .unwrap();
            }
        }

        // The daemon restarts over the same graph. PTYs outlive it, except one that exited
        // while it was down.
        let store = Arc::new(Store::open(&database, "node").unwrap());
        let runtime = Arc::new(FakeRuntime::default());
        runtime
            .ptys
            .lock()
            .unwrap()
            .extend(["running", "starting"].map(pty));
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();

        // Live seats are adopted as they are: nothing is stopped, killed, or started twice.
        assert_eq!(*runtime.starts.lock().unwrap(), ["node.vanished"]);
        assert!(runtime.stops.lock().unwrap().is_empty());
        assert!(runtime.kills.lock().unwrap().is_empty());
        for name in ["running", "starting"] {
            assert_eq!(
                latest_runtime(&store, name),
                ("running".into(), Some(format!("{name}-one")))
            );
        }
        assert_eq!(latest_runtime(&store, "vanished").0, "starting");

        // The starting seat's driver reports ready after the restart; no seat is left waiting.
        store
            .append_claim(&harness_claim(
                "agent/node.starting",
                "ready",
                "starting-one",
            ))
            .unwrap();
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            incarnation_id: Some("vanished-two".into()),
            ..pty("vanished")
        });
        store
            .append_claim(&harness_claim(
                "agent/node.vanished",
                "ready",
                "vanished-two",
            ))
            .unwrap();
        reconciler.reconcile_once().unwrap();
        for (name, incarnation) in [
            ("running", "running-one"),
            ("starting", "starting-one"),
            ("vanished", "vanished-two"),
        ] {
            let harness = store
                .current_harness(&format!("agent/node.{name}"))
                .unwrap()
                .unwrap();
            assert_eq!(harness.incarnation_id, incarnation, "{name}");
            assert!(harness.is_ready(), "{name}");
            assert_eq!(
                latest_runtime(&store, name),
                ("running".into(), Some(incarnation.into()))
            );
        }
        assert_eq!(runtime.starts.lock().unwrap().len(), 1);
        assert!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_later_ready_incarnation_resolves_the_readiness_attention_of_the_one_it_replaced() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{ prompt \"Wait.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "superseded-readiness");
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let member = desired.member.as_ref().unwrap();
        let observe = |incarnation: &str| {
            let claim = store
                .append_claim(&ClaimInput {
                    subject: desired.subject.clone(),
                    kind: "runtime.observed".into(),
                    actor: None,
                    fields: member_fields(member, "running", Some(incarnation), true),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("runtime-{incarnation}")),
                })
                .unwrap();
            let observation = RuntimeObservation {
                runtime_id: member.runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some(incarnation.into()),
            };
            (claim, observation)
        };
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        // The first incarnation never became ready, for example behind a prompt.
        let (claim, stuck) = observe("worker-one");
        let late = claim.accepted_at_unix_ms + HARNESS_READINESS_DEADLINE_MS + 1;
        reconciler
            .reconcile_driver_readiness(&desired, member, &stuck, late)
            .unwrap();
        assert_eq!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .len(),
            1
        );

        // An operator restarts the seat and its next incarnation becomes ready.
        let (_, replacement) = observe("worker-two");
        store
            .append_claim(&harness_claim(&desired.subject, "ready", "worker-two"))
            .unwrap();
        reconciler
            .reconcile_driver_readiness(&desired, member, &replacement, late + 1)
            .unwrap();
        assert!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_replaced_executable_launches_from_its_installed_path() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("st3");
        let deleted = directory.path().join("st3 (deleted)");
        assert_eq!(replaced_executable(&deleted), None);
        std::fs::write(&installed, b"").unwrap();
        assert_eq!(replaced_executable(&deleted), Some(installed.clone()));
        assert_eq!(replaced_executable(&installed), None);
    }

    #[test]
    fn the_recorder_leads_a_member_path_after_its_declaration() {
        let mut environment = BTreeMap::from([(
            "PATH".to_owned(),
            "/workspace/bin:/state/recorder/bin:/usr/bin".to_owned(),
        )]);
        record_member_commands(&mut environment, Some(Path::new("/state/recorder/bin"))).unwrap();
        assert_eq!(
            environment["PATH"],
            "/state/recorder/bin:/workspace/bin:/usr/bin"
        );

        let mut unrecorded = BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]);
        record_member_commands(&mut unrecorded, None).unwrap();
        assert_eq!(unrecorded["PATH"], "/usr/bin");
    }

    #[test]
    fn a_readiness_deadline_alerts_once_without_restarting_and_then_resolves() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{ prompt \"Wait.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "readiness-deadline");
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let member = desired.member.as_ref().unwrap();
        let observation = RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        };
        let runtime_claim = store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: member_fields(member, "running", Some("worker-one"), true),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("runtime-worker-one".into()),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let (event_notify, _) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_event_notify(event_notify.clone());
        let after_deadline = runtime_claim.accepted_at_unix_ms + HARNESS_READINESS_DEADLINE_MS + 1;

        reconciler
            .reconcile_driver_readiness(&desired, member, &observation, after_deadline)
            .unwrap();
        let first_generation = *event_notify.borrow();
        reconciler
            .reconcile_driver_readiness(&desired, member, &observation, after_deadline + 1)
            .unwrap();
        assert_eq!(*event_notify.borrow(), first_generation);
        assert_eq!(
            store
                .observations_for("agent/node.worker", "runtime.readiness-deadline-reached")
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .claims_for(
                    "agent/node.worker",
                    Some("runtime.readiness-deadline-reached")
                )
                .unwrap()
                .is_empty(),
            "the deadline stays on this node; its attention request replicates"
        );
        let attention = store.attention_items(Some("person/operator")).unwrap();
        assert_eq!(attention.len(), 1);
        assert!(runtime.stops.lock().unwrap().is_empty());
        assert!(runtime.kills.lock().unwrap().is_empty());
        assert!(runtime.starts.lock().unwrap().is_empty());

        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-one-ready".into()),
            })
            .unwrap();
        reconciler
            .reconcile_driver_readiness(&desired, member, &observation, after_deadline + 2)
            .unwrap();
        assert!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .claims_for(&attention[0].subject, Some("attention.resolved"))
                .unwrap()[0]
                .actor
                .as_deref(),
            Some("daemon/runtime")
        );
    }

    fn mark_harness_ready(
        store: &Store,
        subject: &str,
        member: &MemberSpec,
        driver: &str,
        incarnation: &str,
    ) {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: member_fields(member, "running", Some(incarnation), true),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{incarnation}-running")),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String(driver.into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{incarnation}-ready")),
            })
            .unwrap();
    }

    fn running_observation(member: &MemberSpec, incarnation: &str) -> RuntimeObservation {
        RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some(incarnation.into()),
        }
    }

    fn alert_resolution_actor(store: &Store, key: &str) -> Option<String> {
        let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
        let attention = store
            .attention_request(&format!("attention/{}", &digest[..32]))
            .unwrap()
            .expect("the alert was raised");
        store
            .claims_for(&attention.subject, Some("attention.resolved"))
            .unwrap()
            .last()
            .and_then(|claim| claim.actor.clone())
    }

    #[test]
    fn a_restarted_seat_resolves_the_previous_incarnations_readiness_alert() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{ prompt \"Wait.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "restarted-seat");
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let member = desired.member.as_ref().unwrap();
        let runtime_claim = store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: member_fields(member, "running", Some("worker-one"), true),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("runtime-worker-one".into()),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let after_deadline = runtime_claim.accepted_at_unix_ms + HARNESS_READINESS_DEADLINE_MS + 1;
        reconciler
            .reconcile_driver_readiness(
                &desired,
                member,
                &running_observation(member, "worker-one"),
                after_deadline,
            )
            .unwrap();
        assert_eq!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .len(),
            1
        );

        // The seat restarts as a new incarnation, and that one becomes ready.
        mark_harness_ready(&store, &desired.subject, member, "codex", "worker-two");
        reconciler
            .reconcile_driver_readiness(
                &desired,
                member,
                &running_observation(member, "worker-two"),
                after_deadline + 1,
            )
            .unwrap();

        assert!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            alert_resolution_actor(&store, "harness-readiness:agent/node.worker:worker-one")
                .as_deref(),
            Some("daemon/runtime")
        );
    }

    #[test]
    fn a_new_desired_revision_that_becomes_ready_resolves_the_codex_crash_loop_alert() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = |prompt: &str| {
            format!(
                "version 2\nagent \"worker\" {{ workspace {:?}; harness \"codex\" {{ prompt {prompt:?} }} }}\n",
                workspace.path().display().to_string()
            )
        };
        apply_source(&store, &source("Wait."), "crash-loop-a");
        let token_a = store
            .selected_desired_token("agent/node.worker")
            .unwrap()
            .unwrap();
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler
            .raise_codex_crash_loop("agent/node.worker", &token_a, "the start failed")
            .unwrap();
        let key = format!("codex-crash-loop:agent/node.worker:{token_a}");
        assert_eq!(alert_resolution_actor(&store, &key), None);

        // A person revises the declaration, and the new revision's incarnation becomes ready.
        apply_source(&store, &source("Wait for work."), "crash-loop-b");
        assert_ne!(
            store.selected_desired_token("agent/node.worker").unwrap(),
            Some(token_a.clone())
        );
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let member = desired.member.as_ref().unwrap();
        mark_harness_ready(&store, &desired.subject, member, "codex", "worker-b");
        reconciler
            .reconcile_driver_readiness(
                &desired,
                member,
                &running_observation(member, "worker-b"),
                now_ms(),
            )
            .unwrap();

        assert_eq!(
            alert_resolution_actor(&store, &key).as_deref(),
            Some("daemon/runtime")
        );
        assert!(
            store
                .attention_items(Some("person/nathan"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn removing_an_agent_resolves_its_reconciler_alerts() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; command "true"; restart "never" }
agent "keeper" { workspace "/tmp"; command "true"; restart "never" }
"#,
            "retired-agent-alerts",
        );
        let raise = |agent: &str, actor: &str| {
            let subject = format!("attention/alert-{}", agent.replace('/', "-"));
            store
                .request_attention(
                    &subject,
                    &AttentionRequest {
                        reviewer: "person/operator".into(),
                        title: "An agent harness did not become ready".into(),
                        reason: "the harness did not become ready".into(),
                        severity: "error".into(),
                        targets: vec![agent.into()],
                        actor: actor.into(),
                        idempotency_key: format!("{subject}:requested"),
                    },
                )
                .unwrap();
            subject
        };
        let stopped = raise("agent/node.worker", "agent/st3/reconciler");
        let gone = raise("agent/node.gone", "agent/st3/reconciler");
        let kept = raise("agent/node.keeper", "agent/st3/reconciler");
        let personal = raise("agent/node.elsewhere", "agent/node.keeper");
        apply_source(
            &store,
            "version 2\nstop \"agent/node.worker\"\n",
            "retired-agent-stop",
        );
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );

        reconciler.reconcile_once().unwrap();

        let status = |subject: &str| store.attention_request(subject).unwrap().unwrap().status;
        assert_eq!(status(&stopped), "resolved");
        assert_eq!(status(&gone), "resolved");
        assert_eq!(status(&kept), "pending");
        // Only the reconciler's own alerts close this way; an agent's request stays with it.
        assert_eq!(status(&personal), "pending");
        assert_eq!(
            store
                .claims_for(&stopped, Some("attention.resolved"))
                .unwrap()[0]
                .actor
                .as_deref(),
            Some("daemon/runtime")
        );
    }

    #[test]
    fn a_harness_that_was_ready_does_not_get_a_startup_deadline_when_it_later_degrades() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let source = format!(
            "version 2\nagent \"worker\" {{ workspace {:?}; harness \"omp\" {{ prompt \"Wait.\" }} }}\n",
            workspace.path().display().to_string()
        );
        apply_source(&store, &source, "ready-before-degraded");
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let member = desired.member.as_ref().unwrap();
        let observation = RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        };
        let runtime_claim = store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: member_fields(member, "running", Some("worker-one"), true),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("runtime-worker-one".into()),
            })
            .unwrap();
        for (state, key) in [("idle", "ready"), ("indeterminate", "stale")] {
            store
                .append_claim(&ClaimInput {
                    subject: desired.subject.clone(),
                    kind: "harness.observed".into(),
                    actor: Some(desired.subject.clone()),
                    fields: BTreeMap::from([
                        ("state".into(), Value::String(state.into())),
                        ("driver".into(), Value::String("omp".into())),
                        ("incarnation_id".into(), Value::String("worker-one".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("worker-one-{key}")),
                })
                .unwrap();
        }
        let (event_notify, _) = watch::channel(0_u64);
        let reconciler = Reconciler::new(
            store.clone(),
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        )
        .with_event_notify(event_notify);
        let after_deadline = runtime_claim.accepted_at_unix_ms + HARNESS_READINESS_DEADLINE_MS + 1;
        reconciler
            .reconcile_driver_readiness(&desired, member, &observation, after_deadline)
            .unwrap();
        assert!(
            store
                .observations_for(&desired.subject, "runtime.readiness-deadline-reached")
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .attention_items(Some("person/operator"))
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_closed_work_wake_does_not_spin_reconciliation_or_replay_attempt_one() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; command "true" }
mission "wake" state="ready" {
  goal "Do the work."
  step "report" { assigned-to "agent/node.worker" }
}
"#,
            "closed-wake-source",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "wake".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "closed-wake-run".into(),
            })
            .unwrap();
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        });
        let setup = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        setup.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("closed-wake-ready".into()),
            })
            .unwrap();
        setup.reconcile_once().unwrap();
        let wake = store
            .messages(Some(&desired.subject), true)
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            wake.tags
                .iter()
                .any(|tag| tag.contains(&run.steps[0].subject))
        );
        store
            .append_claim(&ClaimInput {
                subject: wake.subject.clone(),
                kind: "message.staged".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("staged".into())),
                    ("recipient".into(), Value::String(desired.subject.clone())),
                    ("transport".into(), Value::String("codex-channel".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("stage-work-wake".into()),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: wake.subject.clone(),
                kind: "message.closed".into(),
                actor: Some("daemon/runtime".into()),
                fields: BTreeMap::from([("status".into(), Value::String("closed".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("close-work-wake".into()),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("closed-wake-working".into()),
            })
            .unwrap();
        let notify = Arc::new(Notify::new());
        let reconciler = Reconciler::new(store.clone(), runtime, "node".into(), notify.clone());
        let before = store.index().unwrap();
        reconciler
            .reconcile_work_messages(&desired.subject, "worker-one", None)
            .unwrap();
        assert_eq!(store.index().unwrap(), before);
        assert_eq!(
            store.messages(Some(&desired.subject), true).unwrap().len(),
            1
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), notify.notified())
                .await
                .is_err()
        );
        let sent_claim = store
            .latest_claim(&wake.subject, Some("message.sent"))
            .unwrap()
            .unwrap();
        let evidence = work_wake_attempt_evidence(&store, &[(now_ms(), &wake)]).unwrap();
        assert_eq!(evidence, vec![sent_claim.id]);
        store
            .append_claim(&ClaimInput {
                subject: desired.subject,
                kind: "harness.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("code".into(), Value::String("work-wake-exhausted".into())),
                    ("status".into(), Value::String("failed".into())),
                ]),
                evidence,
                expected_subject: None,
                idempotency_key: Some("closed-wake-evidence".into()),
            })
            .unwrap();
    }

    /// One wake message whose close fails does not hold back the agent's next wake.
    #[test]
    fn a_work_message_that_cannot_close_does_not_hold_back_the_next_wake() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "worker" { workspace "/tmp"; command "true" }
mission "task" state="ready" {
  goal "Do the work."
  concurrent-runs max=2
  step "work" { assigned-to "agent/node.worker"; goal "Do it." }
}
"#;
        apply_source(&store, source, "stuck-close-mission");
        let run = |key: &str| {
            store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: "task".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/test".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: key.into(),
                })
                .unwrap()
        };
        let first = run("first-run");
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-one-ready".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        let first_wake = store.messages(Some("agent/node.worker"), true).unwrap();
        assert_eq!(first_wake.len(), 1);
        // Another request already holds the key the close would use, so the close fails.
        store
            .append_claim(&ClaimInput {
                subject: "daemon/node".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("code".into(), Value::String("occupied".into())),
                    (
                        "reason".into(),
                        Value::String("an unrelated request".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("message-close:{}", first_wake[0].subject)),
            })
            .unwrap();
        store
            .request_mission_run_cancellation(&first.id, "the first run is not needed")
            .unwrap();
        let second = run("second-run");
        for _ in 0..4 {
            reconciler.reconcile_once().unwrap();
        }
        let messages = store.messages(Some("agent/node.worker"), true).unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message.content.contains(&second.steps[0].subject)),
            "the second run's step was never woken: {messages:#?}"
        );
        let fault = store
            .member_reconcile_fault("agent/node.worker", None)
            .unwrap()
            .expect("the failed close is recorded on the agent");
        assert!(fault.contains(&first_wake[0].subject), "{fault}");
    }

    #[test]
    fn the_reconciler_recreates_ready_work_once_for_each_runtime_incarnation() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "worker" { workspace "/tmp"; command "true" }
mission "work-alert" state="ready" {
  goal "Do the work."
  step "work" {
    assigned-to "agent/node.worker"
    title "Build the change"
    goal "Keep this detailed instruction out of the alert."
  }
}
"#;
        apply_source(&store, source, "work-alert-mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "work-alert".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "work-alert-run".into(),
            })
            .unwrap();
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.worker")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("worker-one".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-one-work-ready".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        reconciler.reconcile_once().unwrap();
        let messages = store.messages(Some("agent/node.worker"), true).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].status, "sent");
        assert_eq!(messages[0].from, "daemon/runtime");
        assert_eq!(messages[0].to, "agent/node.worker");
        assert!(messages[0].content.contains(&format!(
            "A mission step is ready: {}",
            run.steps[0].subject
        )));
        assert!(messages[0].content.contains("Title: Build the change"));
        assert!(messages[0].content.contains("Wake: automatic attempt 1"));
        assert!(!messages[0].content.contains("detailed instruction"));
        assert_eq!(
            messages[0].tags[0],
            format!(
                "st3-work:{}@1@1@{}",
                run.steps[0].subject,
                harness_incarnation_key("worker-one")
            )
        );
        assert_eq!(messages[0].tags[1], format!("mission-run:{}", run.subject));
        assert!(messages[0].tags.contains(&"st3-wake-attempt:1".into()));
        assert!(
            messages[0]
                .tags
                .contains(&"st3-wake-source:automatic".into())
        );

        let restarted = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        restarted.reconcile_once().unwrap();
        assert_eq!(
            store
                .messages(Some("agent/node.worker"), true)
                .unwrap()
                .len(),
            1,
            "daemon replay must not duplicate the current wake attempt"
        );

        let replica = Store::open_memory("replica").unwrap();
        replica
            .import_replication("node", &store.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            replica
                .messages(Some("agent/node.worker"), true)
                .unwrap()
                .len(),
            1
        );
        let replicated_step = replica.step_run(&run.steps[0].subject).unwrap().unwrap();
        assert_eq!(replicated_step.wake.unwrap().attempts, 1);

        let stale_message = messages[0].subject.clone();
        store
            .append_claim(&ClaimInput {
                subject: stale_message.clone(),
                kind: "message.staged".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("staged".into())),
                    (
                        "recipient".into(),
                        Value::String("agent/node.worker".into()),
                    ),
                    ("transport".into(), Value::String("claude-channel".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("stage-worker-one-wake".into()),
            })
            .unwrap();

        runtime.ptys.lock().unwrap()[0].incarnation_id = Some("worker-two".into());
        reconciler.reconcile_once().unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-two".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-two-work-ready".into()),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        let messages = store.messages(Some("agent/node.worker"), true).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.status == "sent")
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.status == "closed")
                .count(),
            1
        );
        let withdrawal = store
            .latest_claim(&stale_message, Some("message.closed"))
            .unwrap()
            .unwrap();
        assert_eq!(withdrawal.actor.as_deref(), Some("daemon/runtime"));

        let step = &run.steps[0];
        store
            .work_action(
                &step.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.worker".into()),
                    incarnation: Some("worker-two".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-work-alert".into(),
                },
            )
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            store
                .messages(Some("agent/node.worker"), true)
                .unwrap()
                .iter()
                .filter(|message| message.status == "closed")
                .count(),
            2
        );

        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("worker-three".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("worker-three-idle".into()),
            })
            .unwrap();
        let historical_wake = store
            .step_run(&step.subject)
            .unwrap()
            .unwrap()
            .wake
            .unwrap();
        assert_eq!(historical_wake.attempts, 2);
        assert_eq!(historical_wake.incarnation_id, "worker-two");
        assert_eq!(historical_wake.acknowledged_by.as_deref(), Some("claim"));
    }

    #[test]
    fn unresolved_external_attention_suppresses_ready_work_wakes_until_resolved() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

agent "ios-owner" { workspace "/tmp"; command "true" }
mission "ios-proof-blocked" state="ready" {
  goal "Run the simulator proof."
  step "automated-proof" { assigned-to "agent/node.ios-owner" }
}
"#;
        apply_source(&store, source, "ios-proof-blocked-mission");
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "ios-proof-blocked".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/nathan".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "ios-proof-blocked-run".into(),
            })
            .unwrap();
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/node.ios-owner")
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        runtime.ptys.lock().unwrap().push(RuntimeObservation {
            runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
            terminal: true,
            status: "running".into(),
            exit_code: None,
            incarnation_id: Some("ios-owner-one".into()),
        });
        let reconciler = Reconciler::new(
            store.clone(),
            runtime,
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let subject = run.steps[0].subject.clone();
        let work = |key: &str, reason: Option<&str>| crate::model::WorkRequest {
            actor: Some("agent/node.ios-owner".into()),
            incarnation: Some("ios-owner-one".into()),
            summary: None,
            reason: reason.map(str::to_owned),
            evidence: Vec::new(),
            idempotency_key: key.into(),
        };
        store
            .work_action(&subject, "claim", &work("ios-proof-claim", None))
            .unwrap();
        let attention = store
            .request_attention(
                "attention/ios-proof-xcode",
                &crate::model::AttentionRequest {
                    reviewer: "person/nathan".into(),
                    title: "Silber needs its Xcode simulator components updated".into(),
                    reason: "CoreSimulator cannot start until the privileged repair runs.".into(),
                    severity: "error".into(),
                    targets: vec!["host/silber".into(), subject.clone()],
                    actor: "agent/node.ios-owner".into(),
                    idempotency_key: "ios-proof-xcode-attention".into(),
                },
            )
            .unwrap();
        store
            .work_action(
                &subject,
                "release",
                &work(
                    "ios-proof-release",
                    Some(
                        "A privileged Xcode repair is required; an idle lease would be misleading.",
                    ),
                ),
            )
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: desired.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(desired.subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ready".into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("ios-owner-one".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("ios-owner-ready".into()),
            })
            .unwrap();

        for _ in 0..2 {
            reconciler.reconcile_once().unwrap();
        }
        assert_eq!(store.step_run(&subject).unwrap().unwrap().status, "blocked");
        assert!(
            store
                .messages(Some("agent/node.ios-owner"), true)
                .unwrap()
                .is_empty(),
            "a blocked step must not emit a ready-work wake"
        );

        store
            .resolve_attention(
                &attention.subject,
                &crate::model::AttentionResolveRequest {
                    outcome: "resolved".into(),
                    reason: Some("Xcode simulator components are healthy.".into()),
                    actor: "person/nathan".into(),
                    idempotency_key: "ios-proof-xcode-resolved".into(),
                },
            )
            .unwrap();
        reconciler.reconcile_once().unwrap();
        assert_eq!(store.step_run(&subject).unwrap().unwrap().status, "ready");
        assert_eq!(
            store
                .messages(Some("agent/node.ios-owner"), true)
                .unwrap()
                .len(),
            1,
            "resolving the external blocker must reopen one readiness wake"
        );
    }

    #[test]
    fn inherited_nested_work_keeps_one_parent_alert() {
        let step = |subject: &str, path: &str, assignee: &str| crate::model::StepRunView {
            subject: subject.into(),
            run: "mission-run/run-1".into(),
            generation: "run-generation/run-1".into(),
            step: path.into(),
            fresh_context: false,
            queue: None,
            queue_position: None,
            definition_hash: "definition".into(),
            status: "ready".into(),
            attempt: 1,
            assigned_to: Some(assignee.into()),
            available_to: Vec::new(),
            agentless: false,
            title: None,
            goals: Vec::new(),
            constraints: Vec::new(),
            under: Vec::new(),
            worker_reported: false,
            claimant: None,
            claim_incarnation: None,
            claim_expires_at_unix_ms: None,
            carried_claimant: None,
            execution_started_at_unix_ms: None,
            execution_elapsed_ms: 0,
            timeout_ms: None,
            ready_age_ms: None,
            wake: None,
            progress_summary: None,
            progress_at_unix_ms: None,
            completion_summary: None,
            readiness_epoch: 1,
            blocked_reason: None,
            blockers: Vec::new(),
            not_before_unix_ms: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let parent = step("step-run/run-1/build", "build", "agent/builder");
        let inherited = step(
            "step-run/run-1/build/work/inspect",
            "build/work/inspect",
            "agent/builder",
        );
        let reassigned = step(
            "step-run/run-1/build/work/review",
            "build/work/review",
            "agent/reviewer",
        );
        let work = vec![parent.clone(), inherited.clone(), reassigned.clone()];

        let steps = work
            .iter()
            .map(crate::seat_queue::SeatStep::from)
            .collect::<Vec<_>>();
        assert!(!crate::seat_queue::reached_through_parent(
            &steps[0], &steps
        ));
        assert!(crate::seat_queue::reached_through_parent(&steps[1], &steps));
        assert!(!crate::seat_queue::reached_through_parent(
            &steps[2], &steps
        ));
        assert_eq!(
            next_work_wake_for_agent("agent/builder", &work, &[]),
            Some(parent.subject.as_str())
        );
        assert_eq!(
            next_work_wake_for_agent("agent/reviewer", &work, &[]),
            Some(reassigned.subject.as_str())
        );

        // A claimed parent keeps the seat and carries the inherited alert.
        let mut claimed = parent.clone();
        claimed.status = "claimed".into();
        claimed.claimant = Some("agent/builder".into());
        let work = vec![claimed.clone(), inherited.clone()];
        assert_eq!(next_work_wake_for_agent("agent/builder", &work, &[]), None);

        // A parent submitted before its nested work frees the seat for that nested step.
        let mut submitted = claimed;
        submitted.status = "verifying".into();
        let work = vec![submitted.clone(), inherited.clone()];
        let steps = work
            .iter()
            .map(crate::seat_queue::SeatStep::from)
            .collect::<Vec<_>>();
        assert!(!crate::seat_queue::reached_through_parent(
            &steps[1], &steps
        ));
        assert_eq!(
            next_work_wake_for_agent("agent/builder", &work, &[]),
            Some(inherited.subject.as_str())
        );

        // The wake waits while the turn that submitted the parent is still working.
        let mut harness: CurrentHarnessView = serde_json::from_value(serde_json::json!({
            "state": "working",
            "incarnation_id": "1:turn",
            "claim": "claim/turn",
            "observed_at_unix_ms": 1,
        }))
        .unwrap();
        assert!(defers_inherited_work_wake(
            &inherited,
            &work,
            Some(&harness)
        ));
        let mut nested_with_wake = inherited.clone();
        nested_with_wake.wake = Some(crate::model::WorkWakeView {
            assignee: "agent/builder".into(),
            assignee_state: "working".into(),
            incarnation_id: "1:turn".into(),
            attempts: 0,
            last_attempt_at_unix_ms: None,
            acknowledged_by: None,
            failure: None,
        });
        assert_eq!(
            work_wake_deadline(
                &[submitted.clone(), nested_with_wake],
                &BTreeSet::from(["agent/builder".into()]),
                &BTreeMap::new(),
                5_000,
            ),
            None,
            "the deadline must defer the same nested wake as the sender",
        );
        harness.state = "idle".into();
        assert!(!defers_inherited_work_wake(
            &inherited,
            &work,
            Some(&harness)
        ));
        harness.state = "working".into();
        assert!(!defers_inherited_work_wake(
            &submitted,
            &work,
            Some(&harness)
        ));

        // A parent verifying after its nested work is done still occupies the seat.
        let mut finished = inherited.clone();
        finished.status = "completed".into();
        let work = vec![submitted, finished];
        assert_eq!(next_work_wake_for_agent("agent/builder", &work, &[]), None);
    }

    #[test]
    fn busy_agent_keeps_cross_run_work_queued_without_wake_deadline() {
        let mut first = StepRunView {
            subject: "step-run/older/work".into(),
            run: "mission-run/older".into(),
            generation: "run-generation/older".into(),
            step: "work".into(),
            fresh_context: false,
            queue: None,
            queue_position: None,
            definition_hash: "definition".into(),
            status: "ready".into(),
            attempt: 1,
            assigned_to: Some("agent/worker".into()),
            available_to: Vec::new(),
            agentless: false,
            title: None,
            goals: Vec::new(),
            constraints: Vec::new(),
            under: Vec::new(),
            worker_reported: false,
            claimant: None,
            claim_incarnation: None,
            claim_expires_at_unix_ms: None,
            carried_claimant: None,
            execution_started_at_unix_ms: None,
            execution_elapsed_ms: 0,
            timeout_ms: None,
            ready_age_ms: None,
            wake: Some(crate::model::WorkWakeView {
                assignee: "agent/worker".into(),
                assignee_state: "working".into(),
                incarnation_id: "worker-one".into(),
                attempts: 1,
                last_attempt_at_unix_ms: Some(1_000),
                acknowledged_by: None,
                failure: None,
            }),
            progress_summary: None,
            progress_at_unix_ms: None,
            completion_summary: None,
            readiness_epoch: 1,
            blocked_reason: None,
            blockers: Vec::new(),
            not_before_unix_ms: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        };
        let mut second = first.clone();
        second.subject = "step-run/newer/work".into();
        second.run = "mission-run/newer".into();
        second.created_at_unix_ms = 20;
        let mut active = first.clone();
        active.subject = "step-run/active/work".into();
        active.run = "mission-run/active".into();
        active.status = "working".into();
        active.claimant = Some("agent/worker".into());
        active.wake = None;

        let order = vec![
            "mission-run/active".to_owned(),
            "mission-run/older".to_owned(),
            "mission-run/newer".to_owned(),
        ];
        let orders = BTreeMap::from([("agent/worker".to_owned(), order.clone())]);
        let ready = vec![second.clone(), first.clone()];
        assert_eq!(
            next_work_wake_for_agent("agent/worker", &ready, &order),
            Some(first.subject.as_str())
        );
        assert_eq!(
            work_wake_deadline(
                &ready,
                &BTreeSet::from(["agent/worker".into()]),
                &orders,
                2_000
            ),
            Some(1_000 + WORK_WAKE_RETRY_MS)
        );

        first.wake.as_mut().unwrap().assignee_state = "working".into();
        let busy = vec![second, first, active];
        assert_eq!(
            next_work_wake_for_agent("agent/worker", &busy, &order),
            None
        );
        assert_eq!(
            work_wake_deadline(
                &busy,
                &BTreeSet::from(["agent/worker".into()]),
                &orders,
                2_000
            ),
            None
        );
    }

    #[test]
    fn work_wakes_retry_with_a_bound_and_stop_after_acknowledgement() {
        assert_eq!(
            work_wake_decision(0, None, None, false, 1),
            WorkWakeDecision::Request(1)
        );
        assert_eq!(
            work_wake_decision(
                1,
                Some(1_000),
                Some(1_000),
                false,
                1_000 + WORK_WAKE_RETRY_MS - 1
            ),
            WorkWakeDecision::Wait
        );
        assert_eq!(
            work_wake_decision(
                1,
                Some(1_000),
                Some(1_000),
                false,
                1_000 + WORK_WAKE_RETRY_MS
            ),
            WorkWakeDecision::Request(2)
        );
        assert_eq!(
            work_wake_decision(
                3,
                Some(1_000),
                Some(1_000),
                false,
                1_000 + WORK_WAKE_RETRY_MS
            ),
            WorkWakeDecision::Wait
        );
        assert_eq!(
            work_wake_decision(
                3,
                Some(1_000),
                Some(1_000),
                false,
                1_000 + WORK_WAKE_EXHAUST_GRACE_MS
            ),
            WorkWakeDecision::Exhaust
        );
        assert_eq!(
            work_wake_decision(1, Some(1_000), Some(1_000), true, u128::MAX),
            WorkWakeDecision::Wait
        );
    }

    #[test]
    fn a_read_or_closed_work_wake_acknowledges_a_short_turn() {
        let mut wake = crate::model::MessageView {
            subject: "message/work-wake".into(),
            from: "daemon/runtime".into(),
            to: "agent/worker".into(),
            content: "Claim work".into(),
            status: "sent".into(),
            title: None,
            in_reply_to: None,
            tags: vec![],
            created_index: 1,
        };
        assert!(!work_wake_acknowledged(&[(1_000, &wake)], None));
        wake.status = "delivered".into();
        assert!(!work_wake_acknowledged(&[(1_000, &wake)], None));
        wake.status = "read".into();
        assert!(work_wake_acknowledged(&[(1_000, &wake)], None));
        wake.status = "closed".into();
        assert!(work_wake_acknowledged(&[(1_000, &wake)], None));
    }

    #[test]
    fn a_wake_delivered_into_an_already_working_turn_is_acknowledged() {
        let mut wake = crate::model::MessageView {
            subject: "message/work-wake".into(),
            from: "daemon/runtime".into(),
            to: "agent/worker".into(),
            content: "Claim work".into(),
            status: "delivered".into(),
            title: None,
            in_reply_to: None,
            tags: vec![],
            created_index: 1,
        };
        let mut harness: CurrentHarnessView = serde_json::from_value(serde_json::json!({
            "state": "working",
            "incarnation_id": "1:boot",
            "claim": "claim/boot-turn",
            "observed_at_unix_ms": 900,
        }))
        .unwrap();
        // The boot turn was already working before the wake was requested at 1,000.
        assert!(work_wake_acknowledged(&[(1_000, &wake)], Some(&harness)));
        // A delivered wake to a harness that has since gone idle was not consumed by a turn.
        harness.state = "idle".into();
        assert!(!work_wake_acknowledged(&[(1_000, &wake)], Some(&harness)));
        // An undelivered wake to a busy harness still needs its retry.
        harness.state = "working".into();
        wake.status = "sent".into();
        assert!(!work_wake_acknowledged(&[(1_000, &wake)], Some(&harness)));
    }

    #[test]
    fn staged_pi_wake_into_a_working_turn_is_not_retried() {
        let wake = crate::model::MessageView {
            subject: "message/work-wake".into(),
            from: "daemon/runtime".into(),
            to: "agent/worker".into(),
            content: "Claim work".into(),
            status: "staged".into(),
            title: None,
            in_reply_to: None,
            tags: vec![],
            created_index: 1,
        };
        let harness: CurrentHarnessView = serde_json::from_value(serde_json::json!({
            "state": "working", "driver": "omp", "incarnation_id": "1:turn",
            "claim": "claim/turn", "observed_at_unix_ms": 900,
        }))
        .unwrap();
        assert!(work_wake_acknowledged(&[(1_000, &wake)], Some(&harness)));
    }

    #[test]
    fn an_unchanged_pass_floors_a_past_wake_deadline() {
        assert_eq!(
            deadline_sleep_ms(1_000, 2_000, Some(1_500)),
            WORK_WAKE_RETRY_MS as u64
        );
        assert_eq!(deadline_sleep_ms(3_000, 2_000, Some(1_500)), 1_000);
    }

    /// A mission deadline that falls due while a quiet pass runs has not been evaluated, so the
    /// next pass runs at once rather than after the back-off.
    #[test]
    fn a_deadline_that_falls_due_during_a_quiet_pass_runs_at_once() {
        assert_eq!(deadline_sleep_ms(1_600, 2_000, Some(1_500)), 0);
        assert_eq!(deadline_sleep_ms(1_600, 2_000, None), 0);
    }

    #[test]
    fn a_due_provider_capacity_retry_is_durable_and_does_not_duplicate_after_restart() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; command "true"; restart "never" }
"#,
            "capacity-worker",
        );
        let diagnostic = store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "harness.diagnostic".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("status".into(), Value::String("waiting".into())),
                    ("code".into(), Value::String("provider-capacity".into())),
                    (
                        "reason".into(),
                        Value::String("the selected model is temporarily at capacity".into()),
                    ),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                    ("retry_attempt".into(), Value::from(1)),
                    ("retry_after_unix_ms".into(), Value::from(0)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("capacity-diagnostic".into()),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "runtime.observed".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("worker".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("capacity-runtime".into()),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("worker-one".into())),
                    ("reason".into(), Value::String("providerCapacity".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("capacity-harness".into()),
            })
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        let desired = store.desired_subjects().unwrap();

        reconciler
            .reconcile_provider_capacity_retries(&desired)
            .unwrap();
        let messages = store.messages(Some("agent/node.worker"), true).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon/runtime");
        assert!(messages[0].content.contains("existing session"));
        assert!(
            messages[0]
                .tags
                .contains(&format!("diagnostic-claim:{}", diagnostic.id))
        );

        let restarted = Reconciler::new(
            store.clone(),
            runtime,
            "node".into(),
            Arc::new(Notify::new()),
        );
        restarted
            .reconcile_provider_capacity_retries(&desired)
            .unwrap();
        assert_eq!(
            store
                .messages(Some("agent/node.worker"), true)
                .unwrap()
                .len(),
            1,
            "a daemon restart must not duplicate a scheduled capacity retry"
        );
        assert_eq!(
            restarted.next_provider_capacity_retry_deadline().unwrap(),
            None
        );

        let stale = store
            .append_claim(&ClaimInput {
                subject: "agent/node.worker".into(),
                kind: "harness.diagnostic".into(),
                actor: Some("agent/node.worker".into()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("status".into(), Value::String("waiting".into())),
                    ("code".into(), Value::String("provider-capacity".into())),
                    ("incarnation_id".into(), Value::String("worker-old".into())),
                    ("retry_attempt".into(), Value::from(1)),
                    ("retry_after_unix_ms".into(), Value::from(0)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("stale-capacity-diagnostic".into()),
            })
            .unwrap();
        restarted
            .reconcile_provider_capacity_retries(&desired)
            .unwrap();
        assert_eq!(
            store
                .messages(Some("agent/node.worker"), true)
                .unwrap()
                .len(),
            1,
            "a retry must never cross into a replacement harness incarnation"
        );
        assert!(
            store
                .operation_claim(&provider_capacity_retry_key(&stale.id))
                .unwrap()
                .is_some(),
            "the stale retry must be durably consumed"
        );
    }

    #[test]
    fn only_the_assignee_host_arms_a_work_wake_deadline() {
        let step = StepRunView {
            subject: "step-run/run-1/work".into(),
            run: "mission-run/run-1".into(),
            generation: "run-generation/run-1".into(),
            step: "work".into(),
            fresh_context: false,
            queue: None,
            queue_position: None,
            definition_hash: "definition".into(),
            status: "ready".into(),
            attempt: 1,
            assigned_to: Some("agent/remote.worker".into()),
            available_to: Vec::new(),
            agentless: false,
            title: None,
            goals: Vec::new(),
            constraints: Vec::new(),
            under: Vec::new(),
            worker_reported: false,
            claimant: None,
            claim_incarnation: None,
            claim_expires_at_unix_ms: None,
            carried_claimant: None,
            execution_started_at_unix_ms: None,
            execution_elapsed_ms: 0,
            timeout_ms: None,
            ready_age_ms: Some(1),
            wake: Some(crate::model::WorkWakeView {
                assignee: "agent/remote.worker".into(),
                assignee_state: "ready".into(),
                incarnation_id: "remote-one".into(),
                attempts: 1,
                last_attempt_at_unix_ms: Some(1_000),
                acknowledged_by: None,
                failure: None,
            }),
            progress_summary: None,
            progress_at_unix_ms: None,
            completion_summary: None,
            readiness_epoch: 1,
            blocked_reason: None,
            blockers: Vec::new(),
            not_before_unix_ms: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let work = [step];

        assert_eq!(
            work_wake_deadline(
                &work,
                &BTreeSet::from(["agent/local.worker".into()]),
                &BTreeMap::new(),
                2_000
            ),
            None
        );
        assert_eq!(
            work_wake_deadline(
                &work,
                &BTreeSet::from(["agent/remote.worker".into()]),
                &BTreeMap::new(),
                2_000
            ),
            Some(1_000 + WORK_WAKE_RETRY_MS)
        );
        let mut exhausted = work[0].clone();
        exhausted.wake.as_mut().unwrap().attempts = WORK_WAKE_MAX_ATTEMPTS;
        assert_eq!(
            work_wake_deadline(
                &[exhausted.clone()],
                &BTreeSet::from(["agent/remote.worker".into()]),
                &BTreeMap::new(),
                2_000 + WORK_WAKE_RETRY_MS
            ),
            Some(1_000 + WORK_WAKE_EXHAUST_GRACE_MS),
            "a final wake needs one bounded post-startup check"
        );
        exhausted.wake.as_mut().unwrap().failure = Some("work-wake-exhausted".into());
        assert_eq!(
            work_wake_deadline(
                &[exhausted],
                &BTreeSet::from(["agent/remote.worker".into()]),
                &BTreeMap::new(),
                2_000 + WORK_WAKE_EXHAUST_GRACE_MS
            ),
            None,
            "a recorded failure must not keep the daemon in a busy retry loop"
        );
    }

    const SEAT: &str = "agent/node.worker";

    #[test]
    fn fresh_context_starts_a_new_incarnation_before_a_step_can_be_claimed() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; command "true"; restart "never" }
mission "context" state="ready" {
  goal "Give the worker a clean step."
  step "work" {
    assigned-to "agent/node.worker"
    fresh-context
    goal "Work from the fresh session."
  }
}"#,
            "fresh-context-source",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "context".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "fresh-context-run".into(),
            })
            .unwrap();
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == SEAT)
            .unwrap()
            .member
            .unwrap();
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let step = store.work_for_reconcile(SEAT).unwrap().remove(0);
        assert_eq!(step.subject, run.steps[0].subject);
        assert!(step.fresh_context);
        let claim = |incarnation: &str, key: &str| {
            store.work_action(
                &step.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some(SEAT.into()),
                    incarnation: Some(incarnation.into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: key.into(),
                },
            )
        };

        assert_eq!(
            claim("old", "early-claim").unwrap_err().code,
            "fresh-context-pending"
        );
        let mut ordinary = step.clone();
        ordinary.fresh_context = false;
        assert!(
            reconciler
                .prepare_fresh_context(SEAT, "old", &ordinary, &member)
                .unwrap()
        );
        assert!(runtime.stops.lock().unwrap().is_empty());

        let mut seat_member = member.clone();
        seat_member
            .tags
            .insert("st3.fresh_context".into(), "true".into());
        assert!(
            !reconciler
                .prepare_fresh_context(SEAT, "old", &ordinary, &seat_member)
                .unwrap()
        );
        assert_eq!(runtime.stops.lock().unwrap().len(), 1);

        assert!(
            !reconciler
                .prepare_fresh_context(SEAT, "old", &step, &member)
                .unwrap()
        );
        assert_eq!(runtime.stops.lock().unwrap().len(), 1);
        assert_eq!(
            claim("old", "old-claim").unwrap_err().code,
            "fresh-context-pending"
        );
        assert!(
            reconciler
                .prepare_fresh_context(SEAT, "new", &step, &member)
                .unwrap()
        );
        assert_eq!(
            claim("old", "stale-claim").unwrap_err().code,
            "fresh-context-pending"
        );
        let claimed = claim("new", "new-claim").unwrap();
        assert_eq!(claimed.claim_incarnation.as_deref(), Some("new"));
        assert_eq!(claimed.goals, ["Work from the fresh session."]);
    }

    #[test]
    fn fresh_context_wake_waits_for_the_replacement_harness() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; harness "codex" { model "gpt-6-sol" }; restart "never" }
mission "context-wake" state="ready" {
  goal "Wake a worker in a fresh session."
  step "work" { assigned-to "agent/node.worker"; fresh-context }
}"#,
            "fresh-wake-source",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == SEAT)
            .unwrap()
            .member
            .unwrap();
        store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "context-wake".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "fresh-wake-run".into(),
            })
            .unwrap();
        let observed = |incarnation: &str, status: &str| RuntimeObservation {
            runtime_id: member.runtime_id.clone(),
            terminal: true,
            status: status.into(),
            exit_code: Some(0),
            incarnation_id: Some(incarnation.into()),
        };
        let ready = |incarnation: &str| {
            store
                .append_claim(&ClaimInput {
                    subject: SEAT.into(),
                    kind: "harness.observed".into(),
                    actor: Some(SEAT.into()),
                    fields: BTreeMap::from([
                        ("state".into(), Value::String("ready".into())),
                        ("driver".into(), Value::String("codex".into())),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("ready-{incarnation}")),
                })
                .unwrap();
        };

        *runtime.ptys.lock().unwrap() = vec![observed("old", "running")];
        ready("old");
        reconciler.reconcile_once().unwrap();
        assert_eq!(runtime.stops.lock().unwrap().len(), 1);
        assert!(store.messages(Some(SEAT), false).unwrap().is_empty());

        *runtime.ptys.lock().unwrap() = vec![observed("old", "exited")];
        reconciler.reconcile_once().unwrap();
        assert_eq!(
            runtime.starts.lock().unwrap().len(),
            2,
            "fresh context overrides restart never"
        );
        let launched = runtime.started_members.lock().unwrap();
        let LaunchSpec::Argv(argv) = &launched.last().unwrap().launch else {
            panic!("the replacement harness must have an argv launch");
        };
        assert!(argv.iter().any(|arg| arg == crate::boot::BOOT_PROMPT));
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--resume" || arg == "--continue")
        );
        drop(launched);

        *runtime.ptys.lock().unwrap() = vec![observed("new", "running")];
        ready("new");
        reconciler.reconcile_once().unwrap();
        assert_eq!(store.messages(Some(SEAT), false).unwrap().len(), 1);
    }

    #[test]
    fn seat_fresh_context_covers_a_step_without_its_own_option() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        apply_source(
            &store,
            r#"version 2
agent "worker" { workspace "/tmp"; command "true"; fresh-context }
mission "seat-context" state="ready" {
  goal "Use the seat's fresh-context policy."
  step "work" { assigned-to "agent/node.worker" }
}"#,
            "seat-context-source",
        );
        let runtime = Arc::new(FakeRuntime::default());
        let reconciler = Reconciler::new(
            store.clone(),
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "seat-context".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "seat-context-run".into(),
            })
            .unwrap();
        reconciler.reconcile_once().unwrap();
        let step = store.work_for_reconcile(SEAT).unwrap().remove(0);
        assert!(!step.fresh_context);
        let member = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == SEAT)
            .unwrap()
            .member
            .unwrap();
        assert!(!store.fresh_context_ready(&step, SEAT, "old").unwrap());
        assert!(
            !reconciler
                .prepare_fresh_context(SEAT, "old", &step, &member)
                .unwrap()
        );
        assert_eq!(runtime.stops.lock().unwrap().len(), 1);
        assert!(
            reconciler
                .prepare_fresh_context(SEAT, "new", &step, &member)
                .unwrap()
        );
        assert!(store.fresh_context_ready(&step, SEAT, "new").unwrap());
    }

    const SEAT_QUEUE_SOURCE: &str = r#"
version 2

agent "worker" { workspace "/tmp"; command "true" }
mission "queued" state="ready" {
  concurrent-runs
  goal "Give the durable seat one step in each run."
  step "work" { assigned-to "agent/node.worker" }
}
mission "gated" state="ready" {
  concurrent-runs
  goal "Hold the durable seat's step until another seat finishes."
  step "prepare" { assigned-to "agent/node.helper" }
  step "work" {
    assigned-to "agent/node.worker"
    depends-on { step "prepare" completed }
  }
}
"#;

    struct SeatQueueFixture {
        store: Arc<Store>,
        reconciler: Reconciler<FakeRuntime>,
    }

    impl SeatQueueFixture {
        fn new() -> Self {
            let store = Arc::new(Store::open_memory("node").unwrap());
            apply_source(&store, SEAT_QUEUE_SOURCE, "seat-queue-missions");
            let desired = store
                .desired_subjects()
                .unwrap()
                .into_iter()
                .find(|subject| subject.subject == SEAT)
                .unwrap();
            let runtime = Arc::new(FakeRuntime::default());
            runtime.ptys.lock().unwrap().push(RuntimeObservation {
                runtime_id: desired.member.as_ref().unwrap().runtime_id.clone(),
                terminal: true,
                status: "running".into(),
                exit_code: None,
                incarnation_id: Some("seat-one".into()),
            });
            let reconciler = Reconciler::new(
                store.clone(),
                runtime,
                "node".into(),
                Arc::new(Notify::new()),
            );
            reconciler.reconcile_once().unwrap();
            store
                .append_claim(&ClaimInput {
                    subject: SEAT.into(),
                    kind: "harness.observed".into(),
                    actor: Some(SEAT.into()),
                    fields: BTreeMap::from([
                        ("state".into(), Value::String("ready".into())),
                        ("driver".into(), Value::String("codex".into())),
                        ("incarnation_id".into(), Value::String("seat-one".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some("seat-one-ready".into()),
                })
                .unwrap();
            Self { store, reconciler }
        }

        /// Start a run a little later than the last so join times differ.
        fn start(&self, mission: &str, key: &str) -> crate::model::MissionRunView {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let run = self
                .store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: mission.into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/requester".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: key.into(),
                })
                .unwrap();
            self.reconciler.reconcile_once().unwrap();
            run
        }

        fn step(run: &crate::model::MissionRunView, path: &str) -> String {
            run.steps
                .iter()
                .find(|step| step.step == path)
                .unwrap()
                .subject
                .clone()
        }

        fn queue(&self) -> crate::model::SeatQueueView {
            self.store.seat_queue(SEAT).unwrap()
        }

        fn order(&self) -> Vec<String> {
            self.queue().runs.into_iter().map(|run| run.run).collect()
        }

        fn runs(runs: &[&crate::model::MissionRunView]) -> Vec<String> {
            runs.iter().map(|run| run.subject.clone()).collect()
        }

        /// The next work as the queue view, the roster, and the wake selector see
        /// it. All three come from one selector and must agree.
        fn next(&self) -> Option<String> {
            self.reconciler.reconcile_once().unwrap();
            let queue = self.queue();
            let roster = self
                .store
                .agent_work_queues()
                .unwrap()
                .remove(SEAT)
                .and_then(|queue| queue.next_work_id);
            assert_eq!(queue.next_work_id, roster, "the roster disagrees");
            queue.next_work_id
        }

        /// The step the reconciler wakes now, or none while the seat is held.
        fn wake(&self) -> Option<String> {
            let work = self.store.work_for_reconcile(SEAT).unwrap();
            let order = self.store.seat_run_order(SEAT).unwrap();
            next_work_wake_for_agent(SEAT, &work, &order).map(str::to_owned)
        }

        /// Steps named by open wake messages, oldest first.
        fn woken(&self) -> Vec<String> {
            self.store
                .messages(Some(SEAT), false)
                .unwrap()
                .into_iter()
                .filter_map(|message| {
                    work_message_target(&message).map(|(step, ..)| step.to_owned())
                })
                .collect()
        }

        fn move_run(
            &self,
            run: &str,
            placement: &str,
            anchor: Option<&str>,
            actor: &str,
            key: &str,
        ) -> Result<crate::model::ClaimRecord, crate::model::St3Error> {
            self.store
                .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
                    agent: SEAT.into(),
                    run: run.into(),
                    placement: placement.into(),
                    anchor: anchor.map(str::to_owned),
                    reason: None,
                    actor: actor.into(),
                    idempotency_key: key.into(),
                })
        }

        fn work(&self, step: &str, action: &str, key: &str) -> Result<(), crate::model::St3Error> {
            self.store
                .work_action(
                    step,
                    action,
                    &crate::model::WorkRequest {
                        actor: Some(SEAT.into()),
                        incarnation: Some("seat-one".into()),
                        summary: None,
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: key.into(),
                    },
                )
                .map(|_| ())
        }
    }

    #[test]
    fn exhausted_wake_does_not_disarm_a_later_readiness_epoch() {
        let seat = SeatQueueFixture::new();
        let run = seat.start("queued", "wake-epoch-run");
        let subject = SeatQueueFixture::step(&run, "work");
        let first = seat.store.step_run(&subject).unwrap().unwrap();
        seat.store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "harness.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("code".into(), Value::String("work-wake-exhausted".into())),
                    ("status".into(), Value::String("failed".into())),
                    (
                        "reason".into(),
                        Value::String("first epoch exhausted".into()),
                    ),
                    ("step_run".into(), Value::String(subject.clone())),
                    ("incarnation_id".into(), Value::String("seat-one".into())),
                    ("attempt".into(), Value::from(first.attempt)),
                    ("readiness_epoch".into(), Value::from(first.readiness_epoch)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("wake-epoch-one-exhausted".into()),
            })
            .unwrap();
        seat.work(&subject, "claim", "wake-epoch-claim").unwrap();
        seat.work(&subject, "release", "wake-epoch-release")
            .unwrap();
        let mut next = seat.store.step_run(&subject).unwrap().unwrap();
        assert!(next.readiness_epoch > first.readiness_epoch);
        seat.store
            .populate_work_wake_for_reconcile(&mut next, now_ms())
            .unwrap();
        assert!(next.wake.unwrap().failure.is_none());
    }

    #[test]
    fn delivered_wake_in_an_existing_turn_has_no_retry_deadline() {
        let seat = SeatQueueFixture::new();
        seat.store
            .append_claim(&ClaimInput {
                subject: SEAT.into(),
                kind: "harness.observed".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("working".into())),
                    ("driver".into(), Value::String("omp".into())),
                    ("incarnation_id".into(), Value::String("seat-one".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("seat-one-working".into()),
            })
            .unwrap();
        seat.start("queued", "delivered-existing-turn-run");
        let wake = seat
            .store
            .messages(Some(SEAT), false)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        seat.store
            .append_claim(&ClaimInput {
                subject: wake.subject,
                kind: "message.delivered".into(),
                actor: Some(SEAT.into()),
                fields: BTreeMap::from([("status".into(), Value::String("delivered".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("existing-turn-delivered".into()),
            })
            .unwrap();
        assert_eq!(seat.reconciler.next_work_wake_deadline().unwrap(), None);
    }

    #[test]
    fn manual_wakes_do_not_exhaust_the_reconcilers_automatic_budget() {
        let seat = SeatQueueFixture::new();
        let run = seat
            .store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "queued".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/requester".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "manual-budget-run".into(),
            })
            .unwrap();
        let subject = SeatQueueFixture::step(&run, "work");
        let step = seat.store.step_run(&subject).unwrap().unwrap();
        for n in 1..=3 {
            append_work_wake_message(
                &seat.store,
                &step,
                SEAT,
                "seat-one",
                n,
                "manual",
                "person/operator",
                "explicit retry",
                format!("manual-budget-{n}"),
            )
            .unwrap();
        }
        seat.reconciler.reconcile_once().unwrap();
        let messages = seat.store.messages(Some(SEAT), true).unwrap();
        assert_eq!(
            messages.len(),
            4,
            "one automatic wake follows three manual wakes"
        );
        assert!(messages.iter().any(|message| {
            message
                .tags
                .iter()
                .any(|tag| tag == "st3-wake-source:automatic")
        }));
        assert_eq!(
            seat.store
                .step_run(&subject)
                .unwrap()
                .unwrap()
                .wake
                .unwrap()
                .attempts,
            1
        );
    }

    #[test]
    fn seat_queue_keeps_three_runs_in_start_order_by_default() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-order-first");
        let second = seat.start("queued", "seat-order-second");
        let third = seat.start("queued", "seat-order-third");

        assert_eq!(
            seat.order(),
            SeatQueueFixture::runs(&[&first, &second, &third])
        );
        let queue = seat.queue();
        assert!(queue.runs.iter().all(|run| run.state == "ready"));
        assert_eq!(
            queue
                .runs
                .iter()
                .map(|run| run.position)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let head = SeatQueueFixture::step(&first, "work");
        assert_eq!(seat.next().as_deref(), Some(head.as_str()));
        assert_eq!(seat.wake().as_deref(), Some(head.as_str()));
        assert_eq!(
            seat.woken(),
            std::slice::from_ref(&head),
            "only the head run is woken"
        );

        seat.work(&head, "claim", "seat-order-claim-first").unwrap();
        seat.work(&head, "complete", "seat-order-complete-first")
            .unwrap();
        for _ in 0..8 {
            seat.reconciler.reconcile_once().unwrap();
            if seat.store.mission_run(&first.id).unwrap().unwrap().status == "completed" {
                break;
            }
        }
        assert_eq!(
            seat.order(),
            SeatQueueFixture::runs(&[&second, &third]),
            "a terminal run leaves the queue"
        );
        assert_eq!(
            seat.next().as_deref(),
            Some(SeatQueueFixture::step(&second, "work").as_str())
        );
    }

    #[test]
    fn seat_queue_move_to_top_changes_the_next_claim() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-top-first");
        let second = seat.start("queued", "seat-top-second");
        let third = seat.start("queued", "seat-top-third");
        let original = SeatQueueFixture::step(&first, "work");
        assert_eq!(seat.next().as_deref(), Some(original.as_str()));

        seat.move_run(
            &third.subject,
            "top",
            None,
            "person/operator",
            "seat-top-move",
        )
        .unwrap();
        assert_eq!(
            seat.order(),
            SeatQueueFixture::runs(&[&third, &first, &second])
        );
        let promoted = SeatQueueFixture::step(&third, "work");
        assert_eq!(seat.next().as_deref(), Some(promoted.as_str()));
        assert_eq!(seat.wake().as_deref(), Some(promoted.as_str()));
        assert!(
            seat.woken().contains(&promoted),
            "the reconciler wakes the moved run's step"
        );

        seat.work(&promoted, "claim", "seat-top-claim").unwrap();
        seat.work(&promoted, "complete", "seat-top-complete")
            .unwrap();
        assert_eq!(
            seat.next().as_deref(),
            Some(original.as_str()),
            "the rest of the queue keeps its order"
        );
    }

    #[test]
    fn seat_queue_falls_through_a_waiting_head_run_and_returns_when_it_is_ready() {
        let seat = SeatQueueFixture::new();
        let gated = seat.start("gated", "seat-fall-gated");
        let queued = seat.start("queued", "seat-fall-queued");
        let waiting = SeatQueueFixture::step(&gated, "work");
        let available = SeatQueueFixture::step(&queued, "work");

        assert_eq!(seat.next().as_deref(), Some(available.as_str()));
        assert_eq!(seat.wake().as_deref(), Some(available.as_str()));
        let queue = seat.queue();
        assert_eq!(
            queue.runs[0].run, gated.subject,
            "the gated run keeps its place"
        );
        assert_eq!(queue.runs[0].state, "waiting");
        assert_eq!(
            queue.runs[0].waiting_work_ids,
            std::slice::from_ref(&waiting)
        );
        assert_eq!(queue.runs[1].state, "ready");

        let prepare = SeatQueueFixture::step(&gated, "prepare");
        assert!(
            seat.store
                .set_step_state(&prepare, "completed", None)
                .unwrap()
        );
        assert_eq!(
            seat.next().as_deref(),
            Some(waiting.as_str()),
            "the head run is next again once it has ready work"
        );
        assert_eq!(seat.wake().as_deref(), Some(waiting.as_str()));
        assert_eq!(seat.queue().runs[0].state, "ready");
    }

    #[test]
    fn seat_queue_move_never_preempts_a_held_claim() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-held-first");
        let second = seat.start("queued", "seat-held-second");
        let held = SeatQueueFixture::step(&first, "work");
        let queued = SeatQueueFixture::step(&second, "work");
        assert_eq!(seat.next().as_deref(), Some(held.as_str()));
        seat.work(&held, "claim", "seat-held-claim").unwrap();

        seat.move_run(
            &second.subject,
            "before",
            Some(&first.subject),
            "person/operator",
            "seat-held-move",
        )
        .unwrap();
        seat.reconciler.reconcile_once().unwrap();
        let step = seat.store.step_run(&held).unwrap().unwrap();
        assert_eq!(step.status, "claimed");
        assert_eq!(step.claimant.as_deref(), Some(SEAT));
        let queue = seat.queue();
        assert_eq!(queue.current_work_ids, std::slice::from_ref(&held));
        assert_eq!(queue.runs[0].run, second.subject);
        assert_eq!(queue.runs[1].state, "claimed");
        assert_eq!(seat.next().as_deref(), Some(queued.as_str()));
        assert_eq!(seat.wake(), None, "a held seat is not woken for other work");
        assert!(!seat.woken().contains(&queued));
        assert_eq!(
            seat.work(&queued, "claim", "seat-held-second-claim")
                .unwrap_err()
                .code,
            "agent-capacity"
        );

        seat.work(&held, "complete", "seat-held-complete").unwrap();
        assert_eq!(seat.next().as_deref(), Some(queued.as_str()));
        assert_eq!(seat.wake().as_deref(), Some(queued.as_str()));
    }

    #[test]
    fn seat_queue_refuses_a_claim_from_a_later_run() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-refuse-first");
        let second = seat.start("queued", "seat-refuse-second");
        let head = SeatQueueFixture::step(&first, "work");
        let later = SeatQueueFixture::step(&second, "work");
        assert_eq!(seat.next().as_deref(), Some(head.as_str()));

        let refused = seat.work(&later, "claim", "seat-refuse-later").unwrap_err();
        assert_eq!(refused.code, "seat-queue-order");
        assert!(
            refused.message.contains(&head),
            "the refusal names the next work: {}",
            refused.message
        );
        let step = seat.store.step_run(&later).unwrap().unwrap();
        assert_eq!(step.status, "ready", "a refused claim changes nothing");

        seat.move_run(
            &second.subject,
            "top",
            None,
            "person/operator",
            "seat-refuse-move",
        )
        .unwrap();
        seat.work(&later, "claim", "seat-refuse-later-after-move")
            .unwrap();
        seat.work(&later, "complete", "seat-refuse-later-complete")
            .unwrap();
        seat.work(&head, "claim", "seat-refuse-head").unwrap();
    }

    const REVIEW_DRAINING_SOURCE: &str = r#"
version 2

mission "draining" state="ready" revision-cutover="when-idle" {
  concurrent-runs
  goal "Hold the durable seat's step while another seat's work drains."
  step "helper" { assigned-to "agent/node.helper" }
  step "work" { assigned-to "agent/node.worker" }
}
"#;

    const REVIEW_DRAINING_REVISED: &str = r#"
version 2

mission "draining" state="ready" revision-cutover="when-idle" {
  concurrent-runs
  goal "Hold the durable seat's step while another seat's work drains, revised."
  step "helper" { assigned-to "agent/node.helper" }
  step "work" { assigned-to "agent/node.worker" }
}
"#;

    /// Review 2026-09-27 area 1: the work list, the queue view and the wake hide a
    /// draining run's ready step, so the claim-order check must not name it as the
    /// seat's next work.
    #[test]
    fn review_claim_order_check_agrees_with_the_wake_while_a_head_run_drains() {
        let seat = SeatQueueFixture::new();
        apply_source(&seat.store, REVIEW_DRAINING_SOURCE, "review-draining");
        let head = seat.start("draining", "review-drain-head");
        let later = seat.start("queued", "review-drain-later");
        let helper_step = SeatQueueFixture::step(&head, "helper");
        let head_work = SeatQueueFixture::step(&head, "work");
        let later_work = SeatQueueFixture::step(&later, "work");
        assert_eq!(seat.next().as_deref(), Some(head_work.as_str()));

        // Another seat holds the head run's other step, so a drained cutover waits.
        seat.store
            .set_step_state(&helper_step, "ready", None)
            .unwrap();
        seat.store
            .work_action(
                &helper_step,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some("agent/node.helper".into()),
                    incarnation: Some("helper-one".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "review-helper-claim".into(),
                },
            )
            .unwrap();
        apply_source(
            &seat.store,
            REVIEW_DRAINING_REVISED,
            "review-draining-revised",
        );
        let revised = parse_intent(REVIEW_DRAINING_REVISED, "node")
            .unwrap()
            .missions["draining"]
            .clone();
        let proposal = seat
            .store
            .create_revision_proposal(
                &head.id,
                &revised,
                "person/requester",
                "drain the head run",
                "review-drain-proposal",
            )
            .unwrap();
        assert_eq!(proposal.status, "draining");
        assert_eq!(
            seat.store.mission_run(&head.id).unwrap().unwrap().phase,
            "revision-draining"
        );

        // Every view the seat and the reconciler use now points at the later run.
        let listed = seat
            .store
            .work_for_reconcile(SEAT)
            .unwrap()
            .into_iter()
            .map(|step| step.subject)
            .collect::<Vec<_>>();
        assert!(
            !listed.contains(&head_work),
            "the draining step is hidden: {listed:?}"
        );
        seat.reconciler.reconcile_once().unwrap();
        let queue_next = seat.queue().next_work_id;
        let roster_next = seat
            .store
            .agent_work_queues()
            .unwrap()
            .remove(SEAT)
            .and_then(|queue| queue.next_work_id);
        assert_eq!(queue_next.as_deref(), Some(later_work.as_str()));
        assert_eq!(seat.wake().as_deref(), Some(later_work.as_str()));
        assert_eq!(
            roster_next.as_deref(),
            Some(later_work.as_str()),
            "the roster names the hidden draining step as next work"
        );
        let head_claim = seat
            .work(&head_work, "claim", "review-head-claim")
            .unwrap_err();
        assert_eq!(head_claim.code, "run-generation-draining");

        // The claim check must agree with the wake it was sent.
        if let Err(error) = seat.work(&later_work, "claim", "review-later-claim") {
            panic!(
                "the seat cannot claim the work it was woken for: {} {}",
                error.code, error.message
            );
        }
    }

    #[test]
    fn seat_queue_lets_a_claim_pass_over_a_waiting_head_run() {
        let seat = SeatQueueFixture::new();
        let gated = seat.start("gated", "seat-pass-gated");
        let queued = seat.start("queued", "seat-pass-queued");
        let available = SeatQueueFixture::step(&queued, "work");
        assert_eq!(seat.next().as_deref(), Some(available.as_str()));
        seat.work(&available, "claim", "seat-pass-claim").unwrap();
        assert_eq!(
            seat.queue().runs[0].run,
            gated.subject,
            "the waiting head keeps its place"
        );
    }

    #[test]
    fn a_revision_keeps_the_place_of_work_the_seat_held() {
        let seat = SeatQueueFixture::new();
        let gated = seat.start("gated", "seat-revision-gated");
        let queued = seat.start("queued", "seat-revision-queued");
        let held = SeatQueueFixture::step(&queued, "work");
        let earlier = SeatQueueFixture::step(&gated, "work");
        seat.work(&held, "claim", "seat-revision-claim").unwrap();
        let prepare = SeatQueueFixture::step(&gated, "prepare");
        assert!(
            seat.store
                .set_step_state(&prepare, "completed", None)
                .unwrap()
        );
        seat.reconciler.reconcile_once().unwrap();
        assert_eq!(
            seat.store.step_run(&earlier).unwrap().unwrap().status,
            "ready",
            "the earlier run now has ready work for the seat"
        );
        assert_eq!(
            seat.wake(),
            None,
            "the seat still holds the later run's step"
        );

        apply_source(
            &seat.store,
            &SEAT_QUEUE_SOURCE.replace(
                "Give the durable seat one step in each run.",
                "Give the durable seat one revised step in each run.",
            ),
            "seat-revision-publish",
        );
        let revision = seat.store.mission_spec("queued", None).unwrap().unwrap();
        let revised = seat
            .store
            .adopt_mission_revision(
                &queued.id,
                &revision,
                "person/requester",
                "revise the goal while the seat works",
                "seat-revision-adopt",
            )
            .unwrap();
        let carried = SeatQueueFixture::step(&revised, "work");
        assert_ne!(
            carried, held,
            "the revision carries the step to a new subject"
        );
        let step = seat.store.step_run(&carried).unwrap().unwrap();
        assert_eq!(
            step.status, "claimed",
            "the revision keeps the seat's claim"
        );
        assert_eq!(step.claimant.as_deref(), Some(SEAT));
        assert_eq!(
            seat.order(),
            SeatQueueFixture::runs(&[&gated, &queued]),
            "the revised run keeps its queue position"
        );
        seat.reconciler.reconcile_once().unwrap();
        assert_eq!(seat.wake(), None, "the seat still holds the carried step");
        assert!(!seat.woken().contains(&earlier));
        seat.work(&carried, "complete", "seat-revision-complete")
            .unwrap();
        assert_eq!(
            seat.next().as_deref(),
            Some(earlier.as_str()),
            "the earlier run is next once the carried step is done"
        );
    }

    #[test]
    fn a_released_carried_step_returns_to_queue_order() {
        let seat = SeatQueueFixture::new();
        let gated = seat.start("gated", "seat-release-gated");
        let queued = seat.start("queued", "seat-release-queued");
        let held = SeatQueueFixture::step(&queued, "work");
        let earlier = SeatQueueFixture::step(&gated, "work");
        seat.work(&held, "claim", "seat-release-claim").unwrap();
        let prepare = SeatQueueFixture::step(&gated, "prepare");
        assert!(
            seat.store
                .set_step_state(&prepare, "completed", None)
                .unwrap()
        );
        seat.reconciler.reconcile_once().unwrap();
        assert_eq!(
            seat.store.step_run(&earlier).unwrap().unwrap().status,
            "ready",
            "the earlier run now has ready work for the seat"
        );
        apply_source(
            &seat.store,
            &SEAT_QUEUE_SOURCE.replace(
                "Give the durable seat one step in each run.",
                "Give the durable seat one released step in each run.",
            ),
            "seat-release-publish",
        );
        let revision = seat.store.mission_spec("queued", None).unwrap().unwrap();
        let revised = seat
            .store
            .adopt_mission_revision(
                &queued.id,
                &revision,
                "person/requester",
                "revise the goal while the seat works",
                "seat-release-adopt",
            )
            .unwrap();
        let carried = SeatQueueFixture::step(&revised, "work");
        seat.work(&carried, "claim", "seat-release-reclaim")
            .unwrap();
        seat.work(&carried, "release", "seat-release-release")
            .unwrap();
        assert_eq!(
            seat.next().as_deref(),
            Some(earlier.as_str()),
            "a step the seat gave back waits its turn again"
        );
        assert_eq!(
            seat.work(&carried, "claim", "seat-release-claim-again")
                .unwrap_err()
                .code,
            "seat-queue-order"
        );
    }

    #[test]
    fn seat_order_is_read_only_when_the_seat_chooses_between_runs() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-choice-first");
        let work = |seat: &SeatQueueFixture| seat.store.work_for_reconcile(SEAT).unwrap();
        assert!(
            !seat_chooses_between_runs(SEAT, &work(&seat)),
            "one run leaves nothing to order"
        );

        let second = seat.start("queued", "seat-choice-second");
        assert!(seat_chooses_between_runs(SEAT, &work(&seat)));
        assert!(!seat_chooses_between_runs(
            "agent/node.helper",
            &work(&seat)
        ));

        seat.move_run(
            &second.subject,
            "top",
            None,
            "person/operator",
            "seat-choice-move",
        )
        .unwrap();
        let promoted = SeatQueueFixture::step(&second, "work");
        assert_eq!(seat.next().as_deref(), Some(promoted.as_str()));
        assert!(
            seat.woken().contains(&promoted),
            "the reconciler still reads the order when the seat has a choice"
        );

        seat.work(&promoted, "claim", "seat-choice-claim").unwrap();
        assert!(
            !seat_chooses_between_runs(SEAT, &work(&seat)),
            "a held seat is not woken, so its order is not read"
        );
        seat.work(&promoted, "complete", "seat-choice-complete")
            .unwrap();
        assert_eq!(
            seat.next().as_deref(),
            Some(SeatQueueFixture::step(&first, "work").as_str())
        );
    }

    #[test]
    fn seat_queue_history_names_who_moved_what() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-history-first");
        let second = seat.start("queued", "seat-history-second");
        let third = seat.start("queued", "seat-history-third");
        seat.store
            .move_seat_queue_run(&crate::model::SeatQueueMoveRequest {
                agent: SEAT.into(),
                run: third.subject.clone(),
                placement: "top".into(),
                anchor: None,
                reason: Some("the release needs this first".into()),
                actor: "person/operator-one".into(),
                idempotency_key: "seat-history-top".into(),
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let after = seat
            .move_run(
                &first.subject,
                "after",
                Some(&second.subject),
                "person/operator-two",
                "seat-history-after",
            )
            .unwrap();
        let retry = seat
            .move_run(
                &first.subject,
                "after",
                Some(&second.subject),
                "person/operator-two",
                "seat-history-after",
            )
            .unwrap();
        assert_eq!(retry.id, after.id, "a retried move is one record");

        let queue = seat.queue();
        assert_eq!(
            queue.runs.iter().map(|run| &run.run).collect::<Vec<_>>(),
            [&third.subject, &second.subject, &first.subject]
        );
        assert_eq!(queue.move_count, 2);
        assert_eq!(queue.moves[0].claim_id, after.id);
        assert_eq!(queue.moves[0].actor.as_deref(), Some("person/operator-two"));
        assert_eq!(queue.moves[0].run, first.subject);
        assert_eq!(queue.moves[0].placement, "after");
        assert_eq!(
            queue.moves[0].anchor.as_deref(),
            Some(second.subject.as_str())
        );
        assert_eq!(queue.moves[1].actor.as_deref(), Some("person/operator-one"));
        assert_eq!(queue.moves[1].run, third.subject);
        assert_eq!(queue.moves[1].placement, "top");
        assert_eq!(
            queue.moves[1].reason.as_deref(),
            Some("the release needs this first")
        );
        assert!(queue.moves[0].moved_at_unix_ms >= queue.moves[1].moved_at_unix_ms);
        let claims = seat
            .store
            .claims_for(SEAT, Some(crate::seat_queue::MOVED_CLAIM))
            .unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[1].actor.as_deref(), Some("person/operator-two"));

        let replica = Store::open_memory("replica").unwrap();
        replica
            .import_replication("node", &seat.store.export_replication(0).unwrap())
            .unwrap();
        assert_eq!(
            replica.seat_run_order(SEAT).unwrap(),
            seat.store.seat_run_order(SEAT).unwrap(),
            "a replica rebuilds the same order from the replicated moves"
        );
        assert_eq!(replica.seat_queue(SEAT).unwrap().moves, queue.moves);
    }

    #[test]
    fn a_queue_move_retry_returns_its_claim_after_the_run_ends() {
        let seat = SeatQueueFixture::new();
        let run = seat.start("queued", "queue-retry-run");
        let first = seat
            .move_run(
                &run.subject,
                "top",
                None,
                "person/operator",
                "queue-retry-move",
            )
            .unwrap();
        assert!(
            seat.store
                .set_mission_run_state(&run.subject, "cancelled", "terminal", Some("finished"))
                .unwrap()
        );
        assert!(
            !seat
                .store
                .seat_run_order(SEAT)
                .unwrap()
                .contains(&run.subject)
        );
        let replay = seat
            .move_run(
                &run.subject,
                "top",
                None,
                "person/operator",
                "queue-retry-move",
            )
            .expect("an exact retry returns the recorded move even after the run leaves the queue");
        assert_eq!(replay.id, first.id);
        let changed = seat
            .move_run(
                &run.subject,
                "bottom",
                None,
                "person/operator",
                "queue-retry-move",
            )
            .unwrap_err();
        assert_eq!(changed.code, "idempotency-mismatch");
    }

    #[test]
    fn seat_queue_moves_name_queued_runs_only() {
        let seat = SeatQueueFixture::new();
        let first = seat.start("queued", "seat-invalid-first");
        let second = seat.start("queued", "seat-invalid-second");
        let cases = [
            ("mission-run/absent", "top", None, "run-not-queued"),
            (
                first.subject.as_str(),
                "before",
                Some("mission-run/absent"),
                "run-not-queued",
            ),
            (
                first.subject.as_str(),
                "after",
                None,
                "missing-queue-anchor",
            ),
            (
                first.subject.as_str(),
                "top",
                Some(second.subject.as_str()),
                "unexpected-queue-anchor",
            ),
            (
                first.subject.as_str(),
                "before",
                Some(first.subject.as_str()),
                "invalid-queue-anchor",
            ),
            (
                first.subject.as_str(),
                "middle",
                None,
                "invalid-queue-placement",
            ),
        ];
        for (index, (run, placement, anchor, code)) in cases.into_iter().enumerate() {
            let error = seat
                .move_run(
                    run,
                    placement,
                    anchor,
                    "person/operator",
                    &format!("seat-invalid-{index}"),
                )
                .unwrap_err();
            assert_eq!(error.code, code, "{run} {placement} {anchor:?}");
        }
        assert!(
            seat.store
                .claims_for(SEAT, Some(crate::seat_queue::MOVED_CLAIM))
                .unwrap()
                .is_empty()
        );
    }
}
