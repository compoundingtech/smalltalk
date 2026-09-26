use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use notify::Watcher as _;
use serde_json::Value;
use sha2::Digest as _;
use tokio::sync::{Notify, watch};

use crate::mission::{
    CANDIDATE_INDEX_INPUT, LOOP_FEEDBACK_INPUT, LOOP_ITEM_INPUT, LOOP_ROUND_INPUT,
};
use crate::model::{
    AttentionRequest, ClaimInput, CurrentHarnessView, DependencySpec, DesiredSubject, GateContext,
    GateSpec, LaunchSpec, LoopCandidateSelector, LoopExhaustionSpec, LoopSpec, MemberKind,
    MemberLifecycle, MemberSpec, MessageView, MetricSource, MissionInputKind, MissionRunRequest,
    MissionRunView, MissionSpec, MissionState, RestartIntensity, RestartType, StepRunView,
    StepSpec, UsedMissionSpec, WorkSelector,
};
use crate::resource::{ObservationRequest, RegisteredResourceProvider, ResourceProvider};
use crate::store::Store;

const HARNESS_READINESS_DEADLINE_MS: u128 = 60_000;
const WORK_WAKE_RETRY_MS: u128 = 15_000;
const WORK_WAKE_MAX_ATTEMPTS: u32 = 3;
// A new harness can spend longer than the retry sequence reading its boot
// contract before it claims work. Keep the quick delivery retries, but do not
// diagnose a failed wake while that bounded startup window is still open.
const WORK_WAKE_EXHAUST_GRACE_MS: u128 = 5 * 60_000;
const CODEX_CRASH_LOOP_ATTEMPTS: usize = 3;
const CODEX_CRASH_LOOP_INTERVAL_MS: u128 = 5 * 60_000;

fn claude_login_expired(screen: &str) -> bool {
    screen.contains("Login expired · Please run /login")
        || screen.contains("Not logged in · Run /login")
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
        }
    }
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
        self.pty
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
        let executable = std::env::current_exe()?;
        let environment = st_runtime::materialize_environment(&member.environment, &executable)?;
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
        let cwd = PathBuf::from(&member.cwd);
        if member.terminal {
            let pty_binary = st_runtime::resolve_executable("pty", &environment)?;
            self.pty
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
            self.pty.stop_if(runtime_id, expected_incarnation)
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
            self.pty.kill_if(runtime_id, expected_incarnation)
        } else {
            self.exec.kill_if(runtime_id, expected_incarnation)
        }
    }

    fn remove(&self, runtime_id: &str, terminal: bool) -> Result<()> {
        if terminal {
            if self
                .pty
                .snapshot()?
                .iter()
                .any(|item| item.name == runtime_id)
            {
                self.pty.remove(runtime_id)
            } else {
                Ok(())
            }
        } else {
            self.exec.remove(runtime_id)
        }
    }

    fn attach(&self, runtime_id: &str) -> Result<()> {
        self.pty.attach(runtime_id)
    }

    fn screen(&self, runtime_id: &str) -> Result<String> {
        self.pty.screen(runtime_id)
    }

    fn send_key(&self, runtime_id: &str, key: &str) -> Result<()> {
        self.pty.send_key(runtime_id, key)
    }

    fn read_exec_log(&self, runtime_id: &str) -> Result<Option<String>> {
        self.exec.read_log(runtime_id)
    }
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
    observer_deadlines: Arc<Mutex<HashMap<String, u128>>>,
    observer_cursors: Arc<Mutex<HashMap<String, Option<String>>>>,
    delayed_restarts: Arc<Mutex<HashMap<String, u128>>>,
    file_watchers: Arc<Mutex<HashMap<String, notify::RecommendedWatcher>>>,
    resource_provider: Arc<dyn ResourceProvider>,
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
    ) -> Result<Self> {
        let selected_pty_root = pty_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state_dir.join("pty"));
        Ok(Self {
            store,
            runtime: Arc::new(NativeRuntime::new(
                state_dir,
                Some(&selected_pty_root),
                pty_binary,
            )),
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
            observer_deadlines: Arc::new(Mutex::new(HashMap::new())),
            observer_cursors: Arc::new(Mutex::new(HashMap::new())),
            delayed_restarts: Arc::new(Mutex::new(HashMap::new())),
            file_watchers: Arc::new(Mutex::new(HashMap::new())),
            resource_provider: Arc::new(RegisteredResourceProvider),
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
            observer_deadlines: Arc::new(Mutex::new(HashMap::new())),
            observer_cursors: Arc::new(Mutex::new(HashMap::new())),
            delayed_restarts: Arc::new(Mutex::new(HashMap::new())),
            file_watchers: Arc::new(Mutex::new(HashMap::new())),
            resource_provider: Arc::new(RegisteredResourceProvider),
        }
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
        loop {
            match self.next_reconcile_deadline() {
                Ok(Some(deadline)) => {
                    let delay = deadline.saturating_sub(now_ms()).min(u128::from(u64::MAX)) as u64;
                    tokio::select! {
                        _ = self.notify.notified() => {}
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                    }
                }
                Ok(None) => self.notify.notified().await,
                Err(error) => {
                    let _ = self.record_once(
                        &format!("daemon/{}", self.host),
                        "daemon.diagnostic",
                        BTreeMap::from([
                            ("severity".into(), Value::String("error".into())),
                            ("code".into(), Value::String("deadline-read-failed".into())),
                            ("status".into(), Value::String("indeterminate".into())),
                            ("reason".into(), Value::String(error.to_string())),
                        ]),
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            for pass in 0..64 {
                let before = self.store.index().ok();
                if let Err(error) = self.reconcile_once() {
                    let _ = self.record_once(
                        &format!("daemon/{}", self.host),
                        "daemon.diagnostic",
                        BTreeMap::from([
                            ("severity".into(), Value::String("error".into())),
                            ("code".into(), Value::String("reconcile-failed".into())),
                            ("status".into(), Value::String("unreachable".into())),
                            ("reason".into(), Value::String(error.to_string())),
                        ]),
                    );
                }
                self.event_notify
                    .send_modify(|generation| *generation = generation.saturating_add(1));
                let changed = before != self.store.index().ok();
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

    fn signal_changed(&self) {
        signal_changed(&self.notify, &self.event_notify);
    }

    fn next_reconcile_deadline(&self) -> Result<Option<u128>> {
        Ok([
            self.store.next_active_mission_deadline(&self.host)?,
            self.next_work_wake_deadline()?,
            self.next_provider_capacity_retry_deadline()?,
        ]
        .into_iter()
        .flatten()
        .min())
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
        let candidates = local_agents
            .iter()
            .filter_map(|agent| next_work_wake_for_agent(agent, &work).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let now = now_ms();
        for step in &mut work {
            if candidates.contains(step.subject.as_str()) {
                self.store.populate_work_wake_for_reconcile(step, now)?;
            }
        }
        Ok(work_wake_deadline(&work, &local_agents, now))
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
            Ok(snapshot) => snapshot
                .into_iter()
                .map(|item| (item.runtime_id.clone(), item))
                .collect::<HashMap<_, _>>(),
            Err(error) => {
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
                // An unavailable snapshot is unknown, not an empty runtime set.
                // Treating it as empty could finish run cleanup while a PTY lives.
                self.arm_restart("runtime-snapshot", now_ms().saturating_add(1_000));
                return Ok(());
            }
        };

        let active = desired.iter().collect::<Vec<_>>();
        let mut unavailable_workspaces = BTreeSet::new();
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
            let failure = if member.workspace_create {
                fs::create_dir_all(workspace)
                    .err()
                    .map(|error| error.to_string())
            } else {
                Some("the workspace does not exist and create was not requested".into())
            };
            if let Some(failure) = failure {
                unavailable_workspaces.insert(subject.subject.clone());
                self.record_once(
                    &subject.subject,
                    "harness.diagnostic",
                    BTreeMap::from([
                        ("severity".into(), Value::String("error".into())),
                        ("status".into(), Value::String("unavailable".into())),
                        ("code".into(), Value::String("workspace-unavailable".into())),
                        (
                            "reason".into(),
                            Value::String(format!("workspace {}: {failure}", workspace.display())),
                        ),
                    ]),
                )?;
            }
        }
        let renderable = active
            .iter()
            .copied()
            .filter(|subject| !unavailable_workspaces.contains(&subject.subject))
            .collect::<Vec<_>>();
        let rendered = crate::render::apply_all(&self.store, &renderable, &self.host)?;
        for (subject, result) in rendered {
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
            if !result.receipts.is_empty() {
                self.record_once(
                    &subject,
                    "render.applied",
                    BTreeMap::from([("writes".into(), serde_json::to_value(result.receipts)?)]),
                )?;
            }
        }
        let mut work_message_agents = Vec::new();
        for subject in &active {
            if unavailable_workspaces.contains(&subject.subject) {
                continue;
            }
            if subject.kind == "stop" {
                self.reconcile_stop(subject, &ptys)?;
                continue;
            }
            let Some(member) = &subject.member else {
                continue;
            };
            if member.host != self.host {
                continue;
            }
            let observed = if member.terminal {
                ptys.get(&member.runtime_id).cloned()
            } else {
                self.runtime.observe_exec(&member.runtime_id)?
            };
            match observed {
                Some(observation) if observation.status == "running" => {
                    self.record_member(subject, &observation, true)?;
                    self.reconcile_claude_auth_screen(subject, member, &observation)?;
                    self.reconcile_driver_readiness(subject, member, &observation, now_ms())?;
                    if subject.kind == "agent"
                        && let Some(incarnation) = observation.incarnation_id.as_deref()
                    {
                        work_message_agents.push((subject.subject.clone(), incarnation.to_owned()));
                    }
                }
                Some(observation)
                    if matches!(observation.status.as_str(), "exited" | "vanished") =>
                {
                    self.record_member(subject, &observation, false)?;
                    if !self.member_was_launched_for_selected_desired(&subject.subject)? {
                        self.perform_start(subject, member, "the desired member revision changed")?;
                        continue;
                    }
                    let restart = match member.restart {
                        RestartType::Always => true,
                        RestartType::OnFailure => observation.exit_code != Some(0),
                        RestartType::Never => false,
                    };
                    if restart && member.lifecycle == MemberLifecycle::Service {
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
                        self.perform_start(subject, member, "the desired member revision changed")?;
                        continue;
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
                        if restart {
                            self.reconcile_restart(subject, member, &observation)?;
                        }
                    } else {
                        self.perform_start(subject, member, "the desired member is absent")?;
                    }
                }
            }
        }
        self.reconcile_resource_observers(&desired)?;
        self.reconcile_schedules(&desired)?;
        self.reconcile_scheduled_work(&desired)?;
        self.reconcile_subscription_missions(&desired)?;
        self.reconcile_provider_capacity_retries(&desired)?;
        self.evaluate_mission_runs()?;
        // Mission state is the primary control-plane projection. Evaluate it before
        // wake-message bookkeeping so a large mailbox or work history cannot starve
        // newly-created runs of their first readiness pass.
        for (agent, incarnation) in work_message_agents {
            self.reconcile_work_messages(&agent, &incarnation)?;
        }
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
            if self
                .store
                .attention_request(&attention_subject)?
                .is_some_and(|attention| attention.status == "pending")
            {
                self.store.resolve_attention_automatically(
                    &attention_subject,
                    "the same runtime incarnation became ready",
                    &format!("{attention_key}:resolved"),
                )?;
                self.signal_changed();
            }
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

        let runtime_claim = self
            .store
            .claims_for(&subject.subject, Some("runtime.observed"))?
            .into_iter()
            .rev()
            .find(|claim| {
                claim
                    .body
                    .pointer("/fields/incarnation_id")
                    .and_then(Value::as_str)
                    == Some(incarnation)
                    && claim.body.pointer("/fields/status").and_then(Value::as_str)
                        == Some("running")
            });
        let Some(runtime_claim) = runtime_claim else {
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
            .claims_for(&subject.subject, Some("runtime.readiness-deadline-reached"))?
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
            let key = format!("claude-auth-expired:{subject}:{old_incarnation}");
            let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
            let attention_subject = format!("attention/{}", &digest[..32]);
            if self
                .store
                .attention_request(&attention_subject)?
                .is_some_and(|attention| attention.status == "pending")
            {
                self.store.resolve_attention_automatically(
                    &attention_subject,
                    "a new Claude runtime incarnation became ready",
                    &format!("{key}:resolved"),
                )?;
                self.signal_changed();
            }
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
        if !claude_login_expired(&screen) {
            return Ok(());
        }
        let key = format!("claude-auth-expired:{}:{incarnation}", subject.subject);
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

    fn reconcile_work_messages(&self, agent: &str, incarnation: &str) -> Result<()> {
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

        for message in messages.iter().filter(|message| message.status != "closed") {
            let Some((step_subject, attempt, readiness_epoch, message_incarnation)) =
                work_message_target(message)
            else {
                continue;
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
                continue;
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
                continue;
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
        }

        let next_wake = next_work_wake_for_agent(agent, &work);
        for step in work
            .iter()
            .filter(|step| Some(step.subject.as_str()) == next_wake)
        {
            let tag_value = format!(
                "{}@{}@{}@{}",
                step.subject, step.attempt, step.readiness_epoch, incarnation_key
            );
            let mut attempts = messages
                .iter()
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
        Ok(())
    }

    fn member_was_launched_for_selected_desired(&self, subject: &str) -> Result<bool> {
        let Some(desired_token) = self.store.selected_desired_token(subject)? else {
            return Ok(false);
        };
        Ok(self
            .store
            .claims_for(subject, Some("runtime.action.succeeded"))?
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
        ptys: &HashMap<String, RuntimeObservation>,
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
            .claims_for(subject, Some("runtime.action.requested"))?;
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
            .claims_for(subject, Some("runtime.action.succeeded"))?
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
                .claims_for(&subject.subject, Some("runtime.action.failed"))?
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
        let executable = std::env::current_exe()?;
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
            self.store.append_claim(&ClaimInput {
                subject: subject.subject.clone(),
                kind: "runtime.action.failed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("action".into(), Value::String("start".into())),
                    ("operation".into(), Value::String(operation)),
                    ("reason".into(), Value::String(reason)),
                    ("desired_token".into(), Value::String(desired_token)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })?;
            self.record_once(
                &subject.subject,
                "runtime.observed",
                member_fields(member, "absent", None, false),
            )?;
            return Ok(());
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
                    .expect("restart mutex poisoned")
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
            .claims_for(&subject.subject, Some("runtime.action.succeeded"))?
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
        launches.retain(|claim| claim.store_index > reset_index);

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
            .expect("restart mutex poisoned");
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
                .expect("restart mutex poisoned")
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
            .latest_claim(subject, Some(kind))?
            .is_some_and(|claim| {
                claim.body.get("fields")
                    == Some(&serde_json::to_value(&fields).unwrap_or(Value::Null))
            })
        {
            return Ok(());
        }
        self.store.append_claim(&ClaimInput {
            subject: subject.into(),
            kind: kind.into(),
            actor: None,
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })?;
        self.signal_changed();
        Ok(())
    }

    fn evaluate_mission_runs(&self) -> Result<()> {
        let runs = self.store.active_mission_runs_for_origin(&self.host)?;
        let mut changed = false;
        for run in runs {
            if run
                .deadline_at_unix_ms
                .is_some_and(|deadline| deadline <= now_ms())
                && !run.phase.starts_with("cleanup-")
            {
                let timeout = run.timeout_ms.unwrap_or_default();
                let reason = format!("the mission timeout expired after {timeout}ms");
                changed |= self
                    .store
                    .terminate_mission_run_descendants(&run.id, &reason)?;
                changed |= self.store.set_mission_run_state(
                    &run.id,
                    "running",
                    "cleanup-failed",
                    Some(&reason),
                )?;
                continue;
            }
            if run.phase == "revision-draining" {
                if self.store.apply_drained_revision(&run.id)?.is_some() {
                    changed = true;
                    continue;
                }
                // A replica can receive the old proposal's draining claim after the
                // successor generation. Recover the persisted run phase when no
                // draining proposal still targets its current generation.
                if !self
                    .store
                    .revision_proposal_for_run(&run.id)?
                    .is_some_and(|proposal| proposal.status == "draining")
                {
                    changed |=
                        self.store
                            .set_mission_run_state(&run.id, &run.status, "normal", None)?;
                    continue;
                }
            }
            let mission_id = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
            let Some(mission) = self.store.mission_spec(mission_id, Some(&run.revision))? else {
                changed |= self.store.set_mission_run_state(
                    &run.id,
                    "blocked",
                    &run.phase,
                    Some("the selected mission revision is unavailable"),
                )?;
                continue;
            };
            changed |= self.evaluate_mission_run(&run, &mission)?;
        }
        if changed {
            self.signal_changed();
        }
        Ok(())
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
        changed |= self.materialize_mission_declarations(run, mission)?;
        changed |= self.retire_predecessor_generation(run)?;
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
                let view = views[step.spec.path.as_str()];
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
            let eligible_phase = ((run.phase == "normal" || run.phase == "revision-draining")
                && !step.spec.finally)
                || (matches!(run.phase.as_str(), "final" | "final-cancelled") && step.spec.finally);
            if !eligible_phase {
                continue;
            }
            if run.phase == "revision-draining"
                && matches!(
                    view.status.as_str(),
                    "pending" | "ready" | "blocked" | "failed" | "completed" | "cancelled"
                )
            {
                continue;
            }
            if view.status == "failed" {
                if view.attempt < step.spec.retry.attempts {
                    changed |= self.store.retry_step(
                        &view.subject,
                        "the step repeat policy permits another attempt",
                        step.spec.retry.backoff_ms,
                    )?;
                }
                continue;
            }
            if matches!(view.status.as_str(), "completed" | "cancelled") {
                continue;
            }
            if view.status == "orphaned" {
                changed |= self.store.set_step_state(
                    &view.subject,
                    "ready",
                    Some("the prior worker incarnation ended"),
                )?;
                continue;
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
                continue;
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
                    continue;
                }
                changed |= self.store.set_step_state(
                    &view.subject,
                    "ready",
                    Some("the worker lease expired"),
                )?;
                continue;
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
                continue;
            }
            let assignment_blocked = view.status == "blocked"
                && view
                    .blocked_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("no eligible agent is present"));
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
                    continue;
                }
                if !self.step_dependencies_hold(run, &step, &views)? {
                    continue;
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
                                Some(&format!("step baseline `{}` does not hold", baseline.name)),
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
                    continue;
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
                    changed |= self.materialize_step_declarations(run, &step, view, true)?;
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
                    continue;
                }
                changed |= self.store.set_step_state(&view.subject, "ready", None)?;
                continue;
            }
            if !matches!(
                view.status.as_str(),
                "ready" | "claimed" | "working" | "verifying" | "blocked"
            ) {
                continue;
            }
            // Agentless work has no worker claim to open its execution interval. Admit every
            // eligible agentless step automatically before materializing declarations or waiting
            // on gates, so its timeout and lifecycle are real instead of remaining `ready`
            // forever. Nested missions need the persisted parent transition before their children
            // can be evaluated; other agentless work can materialize in this same pass.
            if view.status == "ready" && view.agentless {
                changed |= self.store.set_step_state(
                    &view.subject,
                    "working",
                    Some("the eligible agentless execution started"),
                )?;
                if step.spec.nested_mission.is_some() {
                    continue;
                }
            }
            changed |= self.materialize_step_declarations(run, &step, view, false)?;
            if let Some(reason) = self.step_declaration_failure(&view.subject)? {
                changed |= self
                    .store
                    .set_step_state(&view.subject, "failed", Some(&reason))?;
                continue;
            }
            if self.step_timed_out(view, &step)? {
                changed |= self.store.set_step_state(
                    &view.subject,
                    "failed",
                    Some("the active execution timeout expired"),
                )?;
                continue;
            }
            if !self.step_declarations_hold(&view.subject)? {
                continue;
            }
            if !view.agentless && !view.worker_reported {
                continue;
            }
            if let Some(loop_spec) = &step.spec.loop_spec {
                changed |= self.evaluate_loop_step(run, &step, view, loop_spec)?;
                continue;
            }
            if let Some(nested) = &step.spec.nested_mission {
                let nested_prefix = format!("{}/{}/", step.spec.path, nested.id);
                if !views
                    .iter()
                    .filter(|(path, _)| path.starts_with(&nested_prefix))
                    .all(|(_, child)| child.status == "completed")
                {
                    continue;
                }
            }
            if step.spec.produces_mission.is_some()
                && !self.produced_mission_holds(step.spec, view)?
            {
                continue;
            }
            if step.spec.uses_mission.is_some() {
                let (use_changed, outcome) =
                    self.evaluate_used_mission(run, &step, view, &views)?;
                changed |= use_changed;
                match outcome {
                    UsedMissionOutcome::Pending => continue,
                    UsedMissionOutcome::Completed => {}
                    UsedMissionOutcome::Failed(reason) => {
                        changed |=
                            self.store
                                .set_step_state(&view.subject, "failed", Some(&reason))?;
                        continue;
                    }
                }
            }
            if !self.products_hold(run, &step, view)? {
                continue;
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
                        changed |=
                            self.store
                                .set_step_state(&view.subject, "failed", Some(&reason))?;
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
        let owned = self
            .store
            .desired_subjects()?
            .into_iter()
            .filter(|subject| {
                subject
                    .owner_run
                    .as_ref()
                    .is_some_and(|owner| owner_runs.contains(owner))
            })
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
            let source = format!("version 2\n\n{}\n", running.join("\n"));
            let intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
            let response = self
                .store
                .apply_internal(&intent, &format!("cleanup-mission-run:{}", run.generation))?;
            return Ok(response.changed);
        }
        if !live.is_empty() {
            return Ok(false);
        }
        let mut status = run.phase.strip_prefix("cleanup-").unwrap_or("failed");
        let mut changed = false;
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
            let mut runtime_cleanup_errors = Vec::new();
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
        changed |= self
            .store
            .set_mission_run_state(&run.id, status, "terminal", None)?;
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
        self.record_once(
            &loop_subject,
            "loop.state",
            BTreeMap::from([
                ("status".into(), Value::String("running".into())),
                ("round".into(), Value::from(view.attempt)),
            ]),
        )?;
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
                    if observation.exit_code != Some(0) {
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
                        execution_started_at_unix_ms: None,
                        execution_elapsed_ms: 0,
                        timeout_ms: None,
                        ready_age_ms: None,
                        wake: None,
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

    fn retire_predecessor_generation(&self, run: &MissionRunView) -> Result<bool> {
        let stops = self
            .store
            .desired_subjects()?
            .into_iter()
            .filter(|subject| subject.owner_run.as_deref() == Some(run.subject.as_str()))
            .filter(|subject| subject.owner_generation.as_deref() != Some(run.generation.as_str()))
            .filter(|subject| subject.member.is_some() && subject.kind != "stop")
            .map(|subject| format!("stop {:?}", subject.subject))
            .collect::<Vec<_>>()
            .join("\n");
        if stops.is_empty() {
            return Ok(false);
        }
        let source = format!("version 2\n\n{stops}\n");
        let mut intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
        for subject in intent.subjects.values_mut() {
            subject.owner_generation = Some(run.generation.clone());
        }
        let response = self
            .store
            .apply_internal(&intent, &format!("retire-generation:{}", run.generation))?;
        Ok(response.changed)
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
        let mut intent = crate::graph::parse_execution_intent(&source, &self.host, &run.id)?;
        let selected = self
            .store
            .desired_subjects()?
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
            .desired_subjects()?
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
            .desired_subjects()?
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
            .desired_subjects()?
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
                .latest_claim(&subject.subject, Some("runtime.action.failed"))?
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

    fn products_hold(
        &self,
        run: &MissionRunView,
        step: &RuntimeStep<'_>,
        view: &crate::model::StepRunView,
    ) -> Result<bool> {
        let variables = run_variables(run, step, view);
        self.products_hold_with_variables(&step.spec.products, &variables)
    }

    fn products_hold_with_variables(
        &self,
        products: &[crate::model::ProductSpec],
        variables: &BTreeMap<String, String>,
    ) -> Result<bool> {
        for product in products {
            let subject = crate::mission::interpolate(&product.subject, variables)?;
            let Some(actual) = self.subject_value(&subject)? else {
                return Ok(false);
            };
            for (field, expected) in &product.fields {
                let expected = match expected {
                    Value::String(value) => {
                        Value::String(crate::mission::interpolate(value, variables)?)
                    }
                    value => value.clone(),
                };
                if actual_field(&actual, field) != Some(&expected) {
                    return Ok(false);
                }
            }
        }
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
        question: Option<&str>,
        review_targets: &[String],
    ) -> Result<GateOutcome> {
        let question = question
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Approve {title}?"));
        let mut fields = BTreeMap::from([
            ("owner".into(), Value::String(subject.to_owned())),
            ("reviewer".into(), Value::String(reviewer.into())),
            ("question".into(), Value::String(question)),
            (
                "review_targets".into(),
                Value::Array(review_targets.iter().cloned().map(Value::String).collect()),
            ),
            (
                "decisions".into(),
                Value::Array(
                    ["approved", "rejected"]
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
            Some("fail") => Ok(GateOutcome::Fail(
                "the human reviewer rejected the work".into(),
            )),
            _ => Ok(GateOutcome::Pending),
        }
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
            let Some(spec) = crate::graph::schedule_spec(&schedule.desired, &self.host) else {
                continue;
            };
            if spec.stopped || spec.host != self.host {
                continue;
            }
            if self.schedule_has_open_work(&schedule.subject)? {
                continue;
            }
            let Some(revision) = self.store.selected_desired_revision(&schedule.subject)? else {
                continue;
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
                    continue;
                }
                (0_u64, at)
            } else {
                let Some(interval) = spec.every_ms else {
                    continue;
                };
                let Some(anchor) = spec.anchor_unix_ms else {
                    continue;
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
                            if remaining > spec.max_catch_up.unwrap_or(0) as u64 {
                                self.record_once(
                                    &schedule.subject,
                                    "runtime.reconcile-decision",
                                    BTreeMap::from([
                                        ("decision".into(), Value::String("raise".into())),
                                        (
                                            "reachability".into(),
                                            Value::String("unreachable".into()),
                                        ),
                                        (
                                            "reason".into(),
                                            Value::String(
                                                "the schedule exceeds max-catch-up".into(),
                                            ),
                                        ),
                                    ]),
                                )?;
                                continue;
                            }
                        }
                        _ => continue,
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
                .expect("schedule mutex poisoned")
                .insert(operation.clone())
            {
                continue;
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
                            .expect("schedule mutex poisoned")
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
                        .expect("schedule mutex poisoned")
                        .remove(&operation);
                    signal_changed(&notify, &event_notify);
                });
            } else {
                self.armed_schedules
                    .lock()
                    .expect("schedule mutex poisoned")
                    .remove(&operation);
            }
        }
        Ok(())
    }

    fn schedule_has_open_work(&self, schedule: &str) -> Result<bool> {
        let requests = self
            .store
            .claims_for(schedule, Some("schedule.work-requested"))?;
        let starts = self
            .store
            .claims_for(schedule, Some("schedule.work-started"))?;
        let Some(request) = requests.into_iter().next_back() else {
            return Ok(false);
        };
        let started = starts.iter().find(|claim| {
            claim
                .body
                .pointer("/fields/request")
                .and_then(Value::as_str)
                == Some(request.id.as_str())
        });
        let Some(started) = started else {
            return Ok(true);
        };
        let Some(run) = started
            .body
            .pointer("/fields/mission_run")
            .and_then(Value::as_str)
        else {
            return Ok(true);
        };
        Ok(self.store.mission_run(run)?.is_some_and(|view| {
            !matches!(view.status.as_str(), "completed" | "cancelled" | "failed")
        }))
    }

    fn reconcile_scheduled_work(&self, desired: &[DesiredSubject]) -> Result<()> {
        for schedule in desired.iter().filter(|item| item.kind == "schedule") {
            let requests = self
                .store
                .claims_for(&schedule.subject, Some("schedule.work-requested"))?;
            let starts = self
                .store
                .claims_for(&schedule.subject, Some("schedule.work-started"))?;
            for request in requests {
                if starts.iter().any(|claim| {
                    claim
                        .body
                        .pointer("/fields/request")
                        .and_then(Value::as_str)
                        == Some(request.id.as_str())
                }) {
                    continue;
                }
                if starts
                    .iter()
                    .filter_map(|claim| {
                        claim
                            .body
                            .pointer("/fields/mission_run")
                            .and_then(Value::as_str)
                    })
                    .any(|run| {
                        self.store
                            .mission_run(run)
                            .ok()
                            .flatten()
                            .is_some_and(|view| {
                                !matches!(
                                    view.status.as_str(),
                                    "completed" | "cancelled" | "failed"
                                )
                            })
                    })
                {
                    continue;
                }
                let Some(mission) = request
                    .body
                    .pointer("/fields/mission")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(revision) = request
                    .body
                    .pointer("/fields/mission_revision")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(root) = request
                    .body
                    .pointer("/fields/workspace")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let suffix = &hex::encode(sha2::Sha256::digest(request.id.as_bytes()))[..16];
                let workspace = Path::new(root).join(suffix).to_string_lossy().into_owned();
                let inputs = serde_json::from_value(
                    request
                        .body
                        .pointer("/fields/inputs")
                        .cloned()
                        .unwrap_or_default(),
                )
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
                let created = schedule
                    .owner_run
                    .as_deref()
                    .and_then(|owner| self.store.mission_run(owner).ok().flatten())
                    .map_or_else(
                        || self.store.create_mission_run(&request_value),
                        |parent| {
                            self.store.create_child_mission_run(
                                &request_value,
                                &parent,
                                &schedule.subject,
                                None,
                            )
                        },
                    );
                let run = match created {
                    Ok(run) => run,
                    Err(error) if error.code == "mission-run-capacity" => continue,
                    Err(error) => return Err(anyhow::anyhow!(error.to_string())),
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
        }
        Ok(())
    }

    fn reconcile_subscription_missions(&self, desired: &[DesiredSubject]) -> Result<()> {
        for item in desired.iter().filter(|item| item.kind == "subscription") {
            let Some(spec) = crate::graph::subscription_spec(&item.desired) else {
                continue;
            };
            if spec.stopped || spec.delivery != "mission" {
                continue;
            }
            let requests = self
                .store
                .claims_for(&item.subject, Some("subscription.mission-requested"))?;
            let starts = self
                .store
                .claims_for(&item.subject, Some("subscription.mission-started"))?;
            for request in requests {
                if starts.iter().any(|claim| {
                    claim
                        .body
                        .pointer("/fields/request")
                        .and_then(Value::as_str)
                        == Some(request.id.as_str())
                }) {
                    continue;
                }
                let fields = request.body.get("fields").unwrap_or(&request.body);
                let Some(mission) = fields.get("mission").and_then(Value::as_str) else {
                    continue;
                };
                let Some(revision) = fields.get("mission_revision").and_then(Value::as_str) else {
                    continue;
                };
                let Some(resource) = fields.get("resource").and_then(Value::as_str) else {
                    continue;
                };
                let Some(discovery) = fields.get("discovery").and_then(Value::as_str) else {
                    continue;
                };
                let Some(input) = fields.get("resource_input").and_then(Value::as_str) else {
                    continue;
                };
                let Some(root) = fields.get("workspace").and_then(Value::as_str) else {
                    continue;
                };
                let requester = fields
                    .get("requester")
                    .and_then(Value::as_str)
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
                let created = item
                    .owner_run
                    .as_deref()
                    .and_then(|owner| self.store.mission_run(owner).ok().flatten())
                    .map_or_else(
                        || self.store.create_mission_run(&request_value),
                        |parent| {
                            self.store.create_child_mission_run(
                                &request_value,
                                &parent,
                                &item.subject,
                                None,
                            )
                        },
                    );
                let run = match created {
                    Ok(run) => run,
                    Err(error) if error.code == "mission-run-capacity" => continue,
                    Err(error) => return Err(anyhow::anyhow!(error.to_string())),
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
        }
        Ok(())
    }

    fn reconcile_resource_observers(&self, desired: &[DesiredSubject]) -> Result<()> {
        let subscriptions = desired
            .iter()
            .filter(|item| item.kind == "subscription")
            .filter_map(|item| {
                crate::graph::subscription_spec(&item.desired)
                    .filter(|spec| !spec.stopped)
                    .map(|spec| (item.subject.clone(), spec))
            })
            .collect::<Vec<_>>();
        for observer in desired.iter().filter(|item| item.kind == "observer") {
            let Some(mut spec) = crate::graph::observer_spec(&observer.desired) else {
                continue;
            };
            let selected = subscriptions
                .iter()
                .filter(|(_, subscription)| subscription.observer == observer.subject)
                .cloned()
                .collect::<Vec<_>>();
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
                continue;
            }
            if self
                .store
                .selected_desired_origin(&observer.subject)?
                .as_deref()
                != Some(self.host.as_str())
            {
                continue;
            }
            spec.fields.extend(
                selected
                    .iter()
                    .flat_map(|(_, subscription)| subscription.fields.iter().cloned()),
            );
            spec.fields.sort();
            spec.fields.dedup();
            let Some(revision) = self.store.selected_desired_revision(&observer.subject)? else {
                continue;
            };
            let observer_actual = self.store.latest_actual_value(&observer.subject)?;
            let refresh_attempt = self
                .store
                .pending_observer_refresh_attempt(&observer.subject)?;
            let deadline_key = format!("{}:{revision}", observer.subject);
            let next_check = refresh_attempt
                .as_ref()
                .map(|_| now_ms())
                .unwrap_or_else(|| {
                    self.observer_deadlines
                        .lock()
                        .expect("observer deadline mutex poisoned")
                        .get(&deadline_key)
                        .copied()
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
                .expect("observer mutex poisoned")
                .insert(operation.clone())
            {
                continue;
            }
            let store = self.store.clone();
            let provider = self.resource_provider.clone();
            let notify = self.notify.clone();
            let event_notify = self.event_notify.clone();
            let armed = self.armed_observers.clone();
            let deadlines = self.observer_deadlines.clone();
            let cursors = self.observer_cursors.clone();
            let observer_subject = observer.subject.clone();
            let previous_facts = self
                .store
                .latest_actual_value(&spec.resource)?
                .and_then(|actual| actual.get("facts").cloned());
            let cursor = self
                .observer_cursors
                .lock()
                .expect("observer cursor mutex poisoned")
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
                            .expect("observer mutex poisoned")
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
                    };
                    match provider.observe(request).await {
                        Ok(observation) => {
                            let _ = store.record_resource_observation(
                                &observer_subject,
                                &revision,
                                refresh_attempt.as_deref(),
                                &spec.resource,
                                observation.cursor.as_deref(),
                                &observation.facts,
                                observation.next_check_unix_ms,
                                &selected,
                            );
                            deadlines
                                .lock()
                                .expect("observer deadline mutex poisoned")
                                .insert(deadline_key.clone(), observation.next_check_unix_ms);
                            cursors
                                .lock()
                                .expect("observer cursor mutex poisoned")
                                .insert(deadline_key.clone(), observation.cursor);
                        }
                        Err(error) => {
                            let retry_at = now_ms().saturating_add(60_000);
                            let reason = error.to_string();
                            deadlines
                                .lock()
                                .expect("observer deadline mutex poisoned")
                                .insert(deadline_key.clone(), retry_at);
                            let unchanged_failure = store
                                .latest_actual_value(&observer_subject)
                                .ok()
                                .flatten()
                                .is_some_and(|actual| {
                                    actual.get("state").and_then(Value::as_str)
                                        == Some("unreachable")
                                        && actual.get("reason").and_then(Value::as_str)
                                            == Some(reason.as_str())
                                });
                            if unchanged_failure && refresh_attempt.is_none() {
                                armed
                                    .lock()
                                    .expect("observer mutex poisoned")
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
                        .expect("observer mutex poisoned")
                        .remove(&operation);
                    signal_changed(&notify, &event_notify);
                });
            } else {
                self.armed_observers
                    .lock()
                    .expect("observer mutex poisoned")
                    .remove(&operation);
            }
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
                    Some("fail") => {
                        GateOutcome::Fail("the human reviewer rejected the work".into())
                    }
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
        let result_subject = gate_operation_subject(
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
                    let verdict = if observation.exit_code == Some(0) {
                        "pass"
                    } else {
                        "fail"
                    };
                    let reason = format!("mechanical gate `{name}` {verdict}");
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

    fn arm_gate_poll(&self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let notify = self.notify.clone();
            handle.spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
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
        let result_subject = gate_operation_subject(
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
            match self.runtime.observe_exec(&runtime_id)? {
                Some(observation) if observation.status == "running" => {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        let notify = self.notify.clone();
                        handle.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            notify.notify_one();
                        });
                    }
                    return Ok(GateOutcome::Pending);
                }
                Some(observation) if observation.status == "indeterminate" => {
                    return Ok(GateOutcome::Pending);
                }
                _ => {}
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
            "{prompt}\n\nYou are a held-out st3 gate. Inspect only the declared workspace and tools. When you decide, run exactly one of these commands:\n  \"$ST3_BIN\" gate-result pass --reason 'REASON'\n  \"$ST3_BIN\" gate-result fail --reason 'REASON'\nDo not finish without posting a gate-result."
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
            .claims_for(subject, Some("runtime.action.succeeded"))?
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
        match std::fs::read(path) {
            Ok(bytes) => {
                let blob_hash = self.store.put_blob(&bytes)?;
                let content_hash = hex::encode(sha2::Sha256::digest(&bytes));
                let mode = std::fs::metadata(path).ok().map(|metadata| {
                    use std::os::unix::fs::PermissionsExt as _;
                    metadata.permissions().mode() & 0o7777
                });
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
            }
            Err(error) => {
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
        let mut watchers = self
            .file_watchers
            .lock()
            .expect("file watcher mutex poisoned");
        if watchers.contains_key(subject) {
            return Ok(());
        }
        let notify = self.notify.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
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

fn signal_changed(reconcile_notify: &Notify, event_notify: &watch::Sender<u64>) {
    reconcile_notify.notify_one();
    event_notify.send_modify(|generation| *generation = generation.saturating_add(1));
}

fn harness_incarnation_key(incarnation: &str) -> String {
    hex::encode(sha2::Sha256::digest(incarnation.as_bytes()))[..12].to_owned()
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
    let content = format!(
        "A mission step is ready: {0}. Run `st3 work claim {0}` to read and claim it.\n\nTitle: {1}{queue}\nWake: {source} attempt {wake_attempt} ({reason})",
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
    now: u128,
) -> Option<u128> {
    work.iter()
        .filter(|step| {
            step.assigned_to
                .as_ref()
                .is_some_and(|assignee| local_agents.contains(assignee))
        })
        .filter(|step| {
            step.assigned_to.as_deref().is_some_and(|agent| {
                next_work_wake_for_agent(agent, work) == Some(step.subject.as_str())
            })
        })
        .filter_map(|step| step.wake.as_ref())
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

/// A maintained harness has one work seat. A claimed step occupies it, while
/// ready steps from all runs wait in one stable, oldest-first queue.
fn next_work_wake_for_agent<'a>(agent: &str, work: &'a [StepRunView]) -> Option<&'a str> {
    if work.iter().any(|step| {
        matches!(step.status.as_str(), "claimed" | "working" | "verifying")
            && (step.claimant.as_deref() == Some(agent)
                || step.assigned_to.as_deref() == Some(agent))
    }) {
        return None;
    }
    work.iter()
        .filter(|step| {
            step.status == "ready"
                && step.assigned_to.as_deref() == Some(agent)
                && should_notify_work_message(step, work)
        })
        .min_by(|left, right| {
            left.created_at_unix_ms
                .cmp(&right.created_at_unix_ms)
                .then_with(|| left.subject.cmp(&right.subject))
        })
        .map(|step| step.subject.as_str())
}

fn should_notify_work_message(
    step: &crate::model::StepRunView,
    work: &[crate::model::StepRunView],
) -> bool {
    !work.iter().any(|candidate| {
        candidate.run == step.run
            && candidate.assigned_to == step.assigned_to
            && candidate.available_to == step.available_to
            && candidate.step.len() < step.step.len()
            && step.step.starts_with(&format!("{}/", candidate.step))
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

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_login_screen_recognizes_only_explicit_auth_prompts() {
        assert!(super::claude_login_expired(
            "● Login expired · Please run /login"
        ));
        assert!(super::claude_login_expired("Not logged in · Run /login"));
        assert!(!super::claude_login_expired(
            "Please use /login to switch accounts"
        ));
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
        started_members: Mutex<Vec<MemberSpec>>,
        stops: Mutex<Vec<String>>,
        kills: Mutex<Vec<String>>,
        removes: Mutex<Vec<String>>,
        screen: Mutex<String>,
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
        fn screen(&self, _runtime_id: &str) -> Result<String> {
            Ok(self.screen.lock().unwrap().clone())
        }
        fn send_key(&self, _runtime_id: &str, key: &str) -> Result<()> {
            self.keys.lock().unwrap().push(key.into());
            Ok(())
        }
        fn read_exec_log(&self, runtime_id: &str) -> Result<Option<String>> {
            Ok(self.logs.lock().unwrap().get(runtime_id).cloned())
        }
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
                .latest_claim(&bad_subject, Some("runtime.action.failed"))
                .unwrap()
                .is_some()
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
            .claims_for("agent/node.bad", Some("harness.diagnostic"))
            .unwrap();
        assert_eq!(diagnostic.len(), 1);
        assert_eq!(
            diagnostic[0]
                .body
                .pointer("/fields/code")
                .and_then(Value::as_str),
            Some("workspace-unavailable")
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
            store,
            runtime.clone(),
            "node".into(),
            Arc::new(Notify::new()),
        );

        let error = reconciler.reconcile_once().unwrap_err();
        assert!(error.to_string().contains("tracked file"));
        assert!(runtime.starts.lock().unwrap().is_empty());
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
        apply_source(&store, &cancellation, "cancel-convergence-stop");
        notify.notify_one();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let current = store.mission_run(&run.id).unwrap().unwrap();
                let cleaned = store
                    .desired_subjects()
                    .unwrap()
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
            store
                .desired_subjects()
                .unwrap()
                .iter()
                .all(|desired| desired.owner_run.as_deref() != Some(run.subject.as_str()))
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

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let current = store.mission_run(&run.id).unwrap().unwrap();
                if current.status == "failed" && current.phase == "terminal" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the daemon did not wake at the mission deadline");
        task.abort();

        let verdict = store
            .latest_claim(&run.subject, Some("eval.verdict"))
            .unwrap()
            .unwrap();
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
        let child = store.mission_run(&child.id).unwrap().unwrap();
        assert_eq!(child.status, "cancelled");
        assert_eq!(child.phase, "terminal");
        assert!(
            store
                .desired_subjects()
                .unwrap()
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
        remote.reconcile_stop(&stop, &HashMap::new()).unwrap();
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
        owner.reconcile_stop(&stop, &HashMap::new()).unwrap();
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
        for _ in 0..5 {
            reconciler.evaluate_mission_runs().unwrap();
        }
        assert_eq!(
            store.mission_run(&occupied.id).unwrap().unwrap().status,
            "completed"
        );
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
    fn one_mission_run_rejects_a_runtime_id_in_two_steps() {
        let store = Arc::new(Store::open_memory("node").unwrap());
        let source = r#"
version 2

  mission "collision" state="ready" {
    goal "Reject two owners for one runtime."
    step "one" { agentless;  exec "same" { command "true"; restart "never" }  }
    step "two" { agentless;  exec "same" { command "true"; restart "never" }  }
  }

"#;
        apply_source(&store, source, "publish-collision");
        store
            .create_mission_run(&MissionRunRequest {
                mission: "collision".into(),
                revision: None,
                workspace: ".".into(),
                requester: None,
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "collision-run".into(),
            })
            .unwrap();
        let reconciler = Reconciler::new(
            store,
            Arc::new(FakeRuntime::default()),
            "node".into(),
            Arc::new(Notify::new()),
        );
        reconciler.reconcile_once().unwrap();
        let error = reconciler.reconcile_once().unwrap_err();
        assert!(error.to_string().contains("more than one mission or step"));
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
                .claims_for(
                    "agent/node.worker",
                    Some("runtime.readiness-deadline-reached")
                )
                .unwrap()
                .len(),
            1
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
                .claims_for(&desired.subject, Some("runtime.readiness-deadline-reached"))
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
            .reconcile_work_messages(&desired.subject, "worker-one")
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
            execution_started_at_unix_ms: None,
            execution_elapsed_ms: 0,
            timeout_ms: None,
            ready_age_ms: None,
            wake: None,
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

        assert!(should_notify_work_message(&parent, &work));
        assert!(!should_notify_work_message(&inherited, &work));
        assert!(should_notify_work_message(&reassigned, &work));
        assert_eq!(
            next_work_wake_for_agent("agent/builder", &work),
            Some(parent.subject.as_str())
        );
        assert_eq!(
            next_work_wake_for_agent("agent/reviewer", &work),
            Some(reassigned.subject.as_str())
        );
    }

    #[test]
    fn busy_agent_keeps_cross_run_work_queued_without_wake_deadline() {
        let mut first = StepRunView {
            subject: "step-run/older/work".into(),
            run: "mission-run/older".into(),
            generation: "run-generation/older".into(),
            step: "work".into(),
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

        let ready = vec![second.clone(), first.clone()];
        assert_eq!(
            next_work_wake_for_agent("agent/worker", &ready),
            Some(first.subject.as_str())
        );
        assert_eq!(
            work_wake_deadline(&ready, &BTreeSet::from(["agent/worker".into()]), 2_000),
            Some(1_000 + WORK_WAKE_RETRY_MS)
        );

        first.wake.as_mut().unwrap().assignee_state = "working".into();
        let busy = vec![second, first, active];
        assert_eq!(next_work_wake_for_agent("agent/worker", &busy), None);
        assert_eq!(
            work_wake_deadline(&busy, &BTreeSet::from(["agent/worker".into()]), 2_000),
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
            readiness_epoch: 1,
            blocked_reason: None,
            blockers: Vec::new(),
            not_before_unix_ms: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        };
        let work = [step];

        assert_eq!(
            work_wake_deadline(&work, &BTreeSet::from(["agent/local.worker".into()]), 2_000),
            None
        );
        assert_eq!(
            work_wake_deadline(
                &work,
                &BTreeSet::from(["agent/remote.worker".into()]),
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
                2_000 + WORK_WAKE_EXHAUST_GRACE_MS
            ),
            None,
            "a recorded failure must not keep the daemon in a busy retry loop"
        );
    }
}
