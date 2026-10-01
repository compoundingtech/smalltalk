use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::harness_state::{Activity, SessionLiveness};
use crate::reconcile::Session;
use crate::residency::{
    Event, Generation, Ledger, RefusalReason, RuntimePresence, RuntimeResidency,
};
use crate::{AgentSpec, ResidencyPolicy, Runner, SessionDriver, TaskLifecycle, UpReport};

const WAKE_SCHEMA: &str = "st2.residency-wake.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPolicy {
    pub idle_after: Duration,
    pub warm_capacity: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WakeRequest {
    schema: String,
    agent_id: String,
    host: String,
    requested_at_ms: u64,
}

const LAUNCH_SCHEMA: &str = "st2.residency-launch.v1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LaunchAttempt {
    schema: String,
    agent_id: String,
    host: String,
    generation: Generation,
    incarnation: String,
}

#[derive(Debug)]
struct LaunchReceipt {
    path: PathBuf,
    agent_id: String,
    host: String,
    driver: SessionDriver,
    generation: Generation,
    task_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Pass {
    launches: Vec<LaunchReceipt>,
}

fn wake_dir(catalog: &Path) -> PathBuf {
    catalog.join(".st2").join("control").join("residency-wake")
}

pub(crate) fn prepare_wake_dir(catalog: &Path) -> std::io::Result<PathBuf> {
    let dir = wake_dir(catalog);
    crate::residency::create_dir_all_durable(&dir)?;
    Ok(dir)
}

fn wake_path(catalog: &Path, host: &str, agent_id: &str) -> PathBuf {
    let mut key = sha2::Sha256::new();
    use sha2::Digest as _;
    for value in [host.as_bytes(), agent_id.as_bytes()] {
        key.update((value.len() as u64).to_be_bytes());
        key.update(value);
    }
    wake_dir(catalog).join(format!("{:x}.json", key.finalize()))
}

fn launch_path(catalog: &Path, host: &str, agent_id: &str) -> PathBuf {
    let file = wake_path(catalog, host, agent_id)
        .file_name()
        .expect("hashed wake path has a file name")
        .to_owned();
    catalog
        .join(".st2")
        .join("control")
        .join("residency-launch")
        .join(file)
}

fn load_launch_attempt(
    catalog: &Path,
    host: &str,
    agent_id: &str,
    generation: Generation,
) -> Result<Option<LaunchAttempt>> {
    let path = launch_path(catalog, host, agent_id);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let attempt: LaunchAttempt = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        attempt.schema == LAUNCH_SCHEMA
            && attempt.agent_id == agent_id
            && attempt.host == host
            && attempt.generation == generation
            && !attempt.incarnation.is_empty(),
        "residency launch attempt ownership, generation, or schema mismatch"
    );
    Ok(Some(attempt))
}

fn prepare_launch_attempt(
    catalog: &Path,
    host: &str,
    agent_id: &str,
    generation: Generation,
    canonical_present: bool,
) -> Result<LaunchAttempt> {
    if canonical_present {
        return load_launch_attempt(catalog, host, agent_id, generation)?
            .context("present residency provider has no launch-attempt fence");
    }
    let attempt = LaunchAttempt {
        schema: LAUNCH_SCHEMA.to_owned(),
        agent_id: agent_id.to_owned(),
        host: host.to_owned(),
        generation,
        incarnation: crate::harness_state::session_token(),
    };
    crate::residency::atomic_json(&launch_path(catalog, host, agent_id), &attempt)?;
    Ok(attempt)
}

pub fn request_wake(catalog: &Path, host: &str, agent_id: &str) -> Result<PathBuf> {
    let path = wake_path(catalog, host, agent_id);
    let request = WakeRequest {
        schema: WAKE_SCHEMA.to_owned(),
        agent_id: agent_id.to_owned(),
        host: host.to_owned(),
        requested_at_ms: crate::message::now_ms(),
    };
    crate::residency::atomic_json(&path, &request)?;
    tracing::info!(
        target: "st2",
        agent_id,
        host,
        requested_at_ms = request.requested_at_ms,
        "residency wake requested"
    );
    Ok(path)
}

fn has_wake_request(catalog: &Path, host: &str, agent_id: &str) -> Result<bool> {
    let path = wake_path(catalog, host, agent_id);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let request: WakeRequest = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        request.schema == WAKE_SCHEMA && request.agent_id == agent_id && request.host == host,
        "residency wake request ownership or schema mismatch"
    );
    Ok(true)
}

fn task_ids(spec: &AgentSpec, host: &str) -> Vec<String> {
    let bus_id = spec.bus_id(host);
    spec.tasks
        .iter()
        .map(|task| crate::reconcile::resolve_task_id(&bus_id, &task.name, task.id.as_deref()))
        .collect()
}

fn canonical_task(spec: &AgentSpec) -> Result<&crate::Task> {
    spec.tasks
        .iter()
        .find(|task| task.name == "agent")
        .context("on-demand agent has no canonical agent task")
}

fn canonical_runtime_id(spec: &AgentSpec, host: &str) -> Result<String> {
    let task = canonical_task(spec)?;
    Ok(crate::reconcile::resolve_task_id(
        &spec.bus_id(host),
        &task.name,
        task.id.as_deref(),
    ))
}

fn provider_argv(task: &crate::Task) -> Result<&[String]> {
    let argv = task
        .argv
        .as_deref()
        .context("on-demand canonical task has no compiled argv")?;
    let runtime = argv
        .iter()
        .position(|argument| argument == "--runtime-id")
        .context("on-demand wrapper argv has no --runtime-id")?;
    anyhow::ensure!(
        runtime + 2 < argv.len(),
        "on-demand wrapper has no provider argv"
    );
    Ok(&argv[runtime + 2..])
}

fn workspace(spec: &AgentSpec, task: &crate::Task, catalog: &Path) -> PathBuf {
    let spec_dir = spec.path.parent().unwrap_or_else(|| Path::new("."));
    match task.cwd.as_deref().or(spec.workspace.as_deref()) {
        Some(path) => spec_dir.join(crate::expand::expand_catalog(path, catalog)),
        None => spec_dir.to_path_buf(),
    }
}

fn inject_resume_fence(
    spec: &mut AgentSpec,
    generation: Generation,
    incarnation: &str,
) -> Result<()> {
    let task = spec
        .tasks
        .iter_mut()
        .find(|task| task.name == "agent")
        .context("on-demand agent has no canonical agent task")?;
    let argv = task
        .argv
        .as_mut()
        .context("on-demand canonical task has no compiled argv")?;
    anyhow::ensure!(
        !argv.iter().any(|argument| {
            argument == "--required-resume-generation"
                || argument == "--required-resume-incarnation"
        }),
        "on-demand wrapper already carries a resume fence"
    );
    let runtime = argv
        .iter()
        .position(|argument| argument == "--runtime-id")
        .context("on-demand wrapper argv has no --runtime-id")?;
    anyhow::ensure!(
        runtime + 1 < argv.len(),
        "on-demand wrapper has no runtime id value"
    );
    argv.splice(
        runtime + 2..runtime + 2,
        [
            "--required-resume-generation".to_owned(),
            generation.0.to_string(),
            "--required-resume-incarnation".to_owned(),
            incarnation.to_owned(),
        ],
    );
    Ok(())
}

fn group_present(ids: &[String], sessions: &[Session]) -> bool {
    ids.iter().all(|id| {
        sessions
            .iter()
            .any(|session| session.pty_id == *id && session.alive)
    })
}

fn group_absent(ids: &[String], sessions: &[Session]) -> bool {
    ids.iter().all(|id| {
        !sessions
            .iter()
            .any(|session| session.pty_id == *id && session.alive)
    })
}

fn group_presence(ids: &[String], sessions: &[Session]) -> RuntimePresence {
    if group_absent(ids, sessions) {
        RuntimePresence::Absent
    } else {
        RuntimePresence::Present
    }
}

fn store_event(path: &Path, ledger: &mut Ledger, event: Event) -> Result<()> {
    let mut next = ledger.clone();
    let transition = next.apply(event);
    crate::residency::store(path, &next).map_err(anyhow::Error::from)?;
    tracing::info!(
        target: "st2",
        agent_id = next.agent_id(),
        host = next.host(),
        generation = next.generation().0,
        event = ?event,
        outcome = ?transition.outcome,
        "residency transition"
    );
    *ledger = next;
    Ok(())
}

fn refuse(
    path: &Path,
    ledger: &mut Ledger,
    presence: RuntimePresence,
    report: &mut UpReport,
    context: &str,
    error: anyhow::Error,
) {
    let generation = ledger.generation();
    ledger.apply(Event::Refused {
        generation,
        reason: RefusalReason::InvalidFence,
        presence,
    });
    if let Err(store_error) = crate::residency::store(path, ledger) {
        report.errors.push(format!(
            "{context}: {error:#}; store residency refusal: {store_error}"
        ));
    } else {
        report.errors.push(format!("{context}: {error:#}"));
    }
}

fn checkpoint(
    catalog: &Path,
    spec: &AgentSpec,
    host: &str,
    driver: SessionDriver,
    source: Generation,
    resume: Generation,
) -> Result<()> {
    let bus_id = spec.bus_id(host);
    let runtime_id = canonical_runtime_id(spec, host)?;
    match driver {
        SessionDriver::Codex => crate::codex_app_server::checkpoint_residency(
            &crate::codex_app_server::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            source,
            resume,
        )
        .map(|_| ()),
        SessionDriver::Omp => crate::omp_session::checkpoint_residency(
            &crate::omp_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            source,
            resume,
        )
        .map(|_| ()),
        SessionDriver::Claude => crate::claude_session::checkpoint_residency(
            &crate::claude_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            source,
            resume,
        )
        .map(|_| ()),
        _ => anyhow::bail!("native residency resume is unsupported for {driver:?}"),
    }
}

fn prepare(
    catalog: &Path,
    spec: &AgentSpec,
    host: &str,
    driver: SessionDriver,
    generation: Generation,
) -> Result<()> {
    let bus_id = spec.bus_id(host);
    let runtime_id = canonical_runtime_id(spec, host)?;
    let task = canonical_task(spec)?;
    let argv = provider_argv(task)?;
    match driver {
        SessionDriver::Codex => crate::codex_app_server::required_residency_resume(
            &crate::codex_app_server::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            generation,
            &argv[1..],
        )
        .map(|_| ()),
        SessionDriver::Omp => crate::omp_session::required_residency_resume(
            &crate::omp_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            generation,
            &argv[1..],
        )
        .map(|_| ()),
        SessionDriver::Claude => crate::claude_session::required_residency_resume(
            &crate::claude_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            &workspace(spec, task, catalog),
            generation,
            argv,
        )
        .map(|_| ()),
        _ => anyhow::bail!("native residency resume is unsupported for {driver:?}"),
    }
}

fn verify_native(
    catalog: &Path,
    spec: &AgentSpec,
    host: &str,
    driver: SessionDriver,
    generation: Generation,
    expected_incarnation: &str,
) -> Result<bool> {
    let bus_id = spec.bus_id(host);
    let runtime_id = canonical_runtime_id(spec, host)?;
    match driver {
        SessionDriver::Codex => crate::codex_app_server::residency_ready(
            &crate::codex_app_server::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            generation,
            expected_incarnation,
        ),
        SessionDriver::Omp => crate::omp_session::residency_ready(
            &crate::omp_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            generation,
            expected_incarnation,
        ),
        SessionDriver::Claude => crate::claude_session::residency_ready(
            &crate::claude_session::state_dir(catalog, &bus_id),
            &bus_id,
            &runtime_id,
            generation,
            expected_incarnation,
        ),
        _ => anyhow::bail!("native residency resume is unsupported for {driver:?}"),
    }
}

fn demand(catalog: &Path, spec: &AgentSpec, host: &str) -> Result<(bool, bool)> {
    let agent_id = spec.effective_id(host);
    let requested = has_wake_request(catalog, host, &agent_id)?;
    let agent_dir = spec.path.parent().unwrap_or_else(|| Path::new("."));
    let inbox = !crate::message::list_inbox(&crate::message::inbox_dir(agent_dir))?.is_empty();
    Ok((requested || inbox, requested))
}

fn idle_since(spec: &AgentSpec, sessions: &[Session]) -> Option<u64> {
    let expected = spec.effective_session_driver()?.as_str();
    let alive = |id: &str| {
        if sessions
            .iter()
            .any(|session| session.pty_id == id && session.alive)
        {
            SessionLiveness::Alive
        } else {
            SessionLiveness::Dead
        }
    };
    let agent_dir = spec.path.parent().unwrap_or_else(|| Path::new("."));
    let observed = crate::harness_state::read(
        &crate::harness_state::harness_state_path(agent_dir),
        Some(&alive),
    )?;
    (observed.state == Activity::Idle && observed.harness.as_deref() == Some(expected))
        .then_some(observed.since_ms)
        .flatten()
}

pub fn before_reconcile(
    catalog: &Path,
    host: &str,
    specs: &mut [AgentSpec],
    sessions: &[Session],
    debounce: &crate::run::LivenessDebounce,
    now: Instant,
    runner: &dyn Runner,
    policy: HostPolicy,
    report: &mut UpReport,
) -> Pass {
    let mut pass = Pass::default();
    let mut ledgers = Vec::new();
    let mut failed_specs = BTreeSet::new();
    for (index, spec) in specs.iter().enumerate() {
        if spec.residency_policy != ResidencyPolicy::OnDemand
            || !spec.desired_state.is_running()
            || spec.resolved_host(host) != host
        {
            continue;
        }
        let Some(driver) = spec.effective_session_driver() else {
            report.errors.push(format!(
                "on-demand agent {} has no native session driver",
                spec.bus_id(host)
            ));
            failed_specs.insert(index);
            continue;
        };
        let agent_id = spec.effective_id(host);
        let path = crate::residency::ledger_path(catalog, host, &agent_id);
        let ledger = match crate::residency::load(&path, &agent_id, host, driver) {
            Ok(Some(ledger)) => ledger,
            Ok(None) => match Ledger::active(&agent_id, host, driver, Generation(1)) {
                Ok(ledger) => {
                    if let Err(error) = crate::residency::store(&path, &ledger) {
                        report
                            .errors
                            .push(format!("initialize residency for {agent_id}: {error}"));
                        failed_specs.insert(index);
                        continue;
                    }
                    ledger
                }
                Err(error) => {
                    report
                        .errors
                        .push(format!("initialize residency for {agent_id}: {error}"));
                    failed_specs.insert(index);
                    continue;
                }
            },
            Err(error) => {
                report
                    .errors
                    .push(format!("load residency for {agent_id}: {error}"));
                failed_specs.insert(index);
                continue;
            }
        };
        ledgers.push((index, path, driver, ledger));
    }

    for index in failed_specs {
        for task in &mut specs[index].tasks {
            task.lifecycle = TaskLifecycle::AdoptOnly;
        }
    }
    let active_count = ledgers
        .iter()
        .filter(|(_, _, _, ledger)| ledger.runtime_residency() == &RuntimeResidency::Active)
        .count();
    let excess = active_count.saturating_sub(policy.warm_capacity);
    let mut idle = ledgers
        .iter()
        .filter_map(|(index, _, _, ledger)| {
            (ledger.runtime_residency() == &RuntimeResidency::Active)
                .then(|| idle_since(&specs[*index], sessions).map(|since| (*index, since)))
                .flatten()
        })
        .filter(|(_, since)| {
            crate::message::now_ms().saturating_sub(*since)
                >= u64::try_from(policy.idle_after.as_millis()).unwrap_or(u64::MAX)
        })
        .collect::<Vec<_>>();
    idle.sort_by_key(|(_, since)| *since);
    let suspend = idle
        .into_iter()
        .take(excess)
        .map(|(index, _)| index)
        .collect::<BTreeSet<_>>();

    for (index, path, driver, mut ledger) in ledgers {
        let ids = task_ids(&specs[index], host);
        let presence = group_presence(&ids, sessions);
        let (has_demand, requested, demand_observed) = match demand(catalog, &specs[index], host) {
            Ok((has_demand, requested)) => (has_demand, requested, true),
            Err(error) => {
                report.errors.push(format!(
                    "observe residency demand for {}: {error:#}",
                    specs[index].bus_id(host)
                ));
                (false, false, false)
            }
        };
        if has_demand {
            if let Err(error) = store_event(&path, &mut ledger, Event::WakeDemandObserved) {
                report.errors.push(format!(
                    "record residency demand for {}: {error:#}",
                    specs[index].bus_id(host)
                ));
            } else if requested {
                let _ = fs::remove_file(wake_path(catalog, host, &specs[index].effective_id(host)));
            }
        }
        if suspend.contains(&index) && !has_demand && demand_observed {
            let generation = ledger.generation();
            if let Err(error) = store_event(&path, &mut ledger, Event::IdleConfirmed { generation })
            {
                report.errors.push(format!(
                    "record idle residency for {}: {error:#}",
                    specs[index].bus_id(host)
                ));
            }
        }

        loop {
            let Some(action) = ledger.next_action() else {
                break;
            };
            let result = match action {
                crate::residency::Action::CheckpointNativeSession { source, resume } => {
                    checkpoint(catalog, &specs[index], host, driver, source, resume).and_then(
                        |()| {
                            store_event(
                                &path,
                                &mut ledger,
                                Event::CheckpointStored { source, resume },
                            )
                        },
                    )
                }
                crate::residency::Action::StopOwnedGroup { generation } => {
                    let mut failed = None;
                    for id in &ids {
                        if sessions
                            .iter()
                            .any(|session| session.pty_id == *id && session.alive)
                            && let Err(error) = runner.kill(id)
                        {
                            let error = error.context(format!("stopping residency task {id}"));
                            if failed.is_none() {
                                failed = Some(error);
                            }
                        }
                    }
                    match failed {
                        Some(error) => Err(error),
                        None => {
                            store_event(&path, &mut ledger, Event::OwnedGroupStopped { generation })
                        }
                    }
                }
                crate::residency::Action::VerifyAbsent { generation } => {
                    if group_absent(&ids, sessions)
                        && ids.iter().all(|id| !debounce.recently_alive(id, now))
                    {
                        store_event(&path, &mut ledger, Event::AbsenceVerified { generation })
                    } else {
                        break;
                    }
                }
                crate::residency::Action::PrepareNativeResume { generation } => {
                    prepare(catalog, &specs[index], host, driver, generation).and_then(|()| {
                        store_event(
                            &path,
                            &mut ledger,
                            Event::NativeResumePrepared { generation },
                        )
                    })
                }
                crate::residency::Action::LaunchOwnedGroup { generation } => {
                    let agent_id = specs[index].effective_id(host);
                    let canonical_id = canonical_runtime_id(&specs[index], host);
                    let canonical_present = canonical_id.as_ref().is_ok_and(|canonical_id| {
                        sessions
                            .iter()
                            .any(|session| session.pty_id == *canonical_id && session.alive)
                    });
                    match canonical_id
                        .and_then(|_| {
                            prepare_launch_attempt(
                                catalog,
                                host,
                                &agent_id,
                                generation,
                                canonical_present,
                            )
                        })
                        .and_then(|attempt| {
                            inject_resume_fence(&mut specs[index], generation, &attempt.incarnation)
                        })
                    {
                        Ok(()) => pass.launches.push(LaunchReceipt {
                            path: path.clone(),
                            agent_id,
                            host: host.to_owned(),
                            driver,
                            generation,
                            task_ids: ids.clone(),
                        }),
                        Err(error) => refuse(
                            &path,
                            &mut ledger,
                            presence,
                            report,
                            "prepare residency launch",
                            error,
                        ),
                    }
                    break;
                }
                crate::residency::Action::VerifyNativeSession { generation } => {
                    if !group_present(&ids, sessions) {
                        break;
                    }
                    let agent_id = specs[index].effective_id(host);
                    let verified = load_launch_attempt(catalog, host, &agent_id, generation)
                        .and_then(|attempt| {
                            attempt.context("residency launch-attempt fence disappeared")
                        })
                        .and_then(|attempt| {
                            if !verify_native(
                                catalog,
                                &specs[index],
                                host,
                                driver,
                                generation,
                                &attempt.incarnation,
                            )? {
                                return Ok(false);
                            }
                            store_event(
                                &path,
                                &mut ledger,
                                Event::NativeSessionVerified { generation },
                            )?;
                            let _ = fs::remove_file(launch_path(catalog, host, &agent_id));
                            Ok(true)
                        });
                    match verified {
                        Ok(true) => Ok(()),
                        Ok(false) => break,
                        Err(error) => Err(error),
                    }
                }
            };
            if let Err(error) = result {
                if matches!(action, crate::residency::Action::StopOwnedGroup { .. }) {
                    report.errors.push(format!(
                        "residency action for {}: {error:#}",
                        specs[index].bus_id(host)
                    ));
                } else {
                    refuse(
                        &path,
                        &mut ledger,
                        presence,
                        report,
                        &format!("residency action for {}", specs[index].bus_id(host)),
                        error,
                    );
                }
                break;
            }
        }

        if !matches!(
            ledger.runtime_residency(),
            RuntimeResidency::Active
                | RuntimeResidency::Starting {
                    step: crate::residency::StartStep::LaunchOwnedGroup,
                    ..
                }
        ) {
            for task in &mut specs[index].tasks {
                task.lifecycle = TaskLifecycle::AdoptOnly;
            }
        }
    }
    pass
}

pub fn after_reconcile(runner: &dyn Runner, pass: Pass, report: &mut UpReport) {
    if pass.launches.is_empty() {
        return;
    }
    let sessions = match runner.list_sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            report
                .errors
                .push(format!("verify launched residency groups: {error}"));
            return;
        }
    };
    for launch in pass.launches {
        if !group_present(&launch.task_ids, &sessions) {
            continue;
        }
        let mut ledger = match crate::residency::load(
            &launch.path,
            &launch.agent_id,
            &launch.host,
            launch.driver,
        ) {
            Ok(Some(ledger)) => ledger,
            Ok(None) => {
                report.errors.push(format!(
                    "verify residency launch for {}: ledger disappeared",
                    launch.agent_id
                ));
                continue;
            }
            Err(error) => {
                report.errors.push(format!(
                    "verify residency launch for {}: {error}",
                    launch.agent_id
                ));
                continue;
            }
        };
        if let Err(error) = store_event(
            &launch.path,
            &mut ledger,
            Event::OwnedGroupLaunched {
                generation: launch.generation,
            },
        ) {
            report.errors.push(format!(
                "record residency launch for {}: {error:#}",
                launch.agent_id
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, alive: bool) -> Session {
        Session {
            pty_id: id.to_owned(),
            alive,
            exit_code: None,
            presentation: None,
        }
    }

    struct FailingKillRunner {
        killed: std::cell::RefCell<Vec<String>>,
    }

    impl Runner for FailingKillRunner {
        fn list_sessions(&self) -> Result<Vec<Session>> {
            unreachable!()
        }

        fn spawn(&self, _target: &crate::reconcile::TaskTarget, _spec_dir: &Path) -> Result<()> {
            unreachable!()
        }

        fn kill(&self, pty_id: &str) -> Result<()> {
            let first = self.killed.borrow().is_empty();
            self.killed.borrow_mut().push(pty_id.to_owned());
            if first {
                anyhow::bail!("injected kill failure")
            }
            Ok(())
        }

        fn remove(&self, _pty_id: &str) -> Result<()> {
            unreachable!()
        }
    }

    fn on_demand_spec(path: PathBuf) -> AgentSpec {
        let task = |name: &str| agent_spec::spec::Task {
            kind: agent_spec::spec::TaskKind::Pty,
            derived: name != "agent",
            name: name.to_owned(),
            id: None,
            command: None,
            argv: Some(vec![
                "wrapper".to_owned(),
                "--runtime-id".to_owned(),
                "runtime".to_owned(),
            ]),
            cwd: None,
            tags: Default::default(),
            env: Default::default(),
            keep: false,
            lifecycle: TaskLifecycle::Service,
        };
        AgentSpec {
            id: None,
            address: None,
            identity: "worker".to_owned(),
            name: None,
            description: None,
            host: Some("host-a".to_owned()),
            role: None,
            job_type: agent_spec::spec::JobType::Service,
            workspace: None,
            supervisor: None,
            desired_state: agent_spec::spec::AgentDesiredState::Running,
            residency_policy: ResidencyPolicy::OnDemand,
            keep: false,
            restart: None,
            delivery: None,
            session_driver: Some(SessionDriver::Omp),
            driver: None,
            delivery_readiness: None,
            resources: Vec::new(),
            streams: Vec::new(),
            tasks: vec![task("agent"), task("sidecar")],
            path,
        }
    }

    #[test]
    fn missing_provider_retries_rotate_the_attempt_fence_and_live_providers_retain_it() {
        let catalog = tempfile::tempdir().unwrap();
        let mut spec = on_demand_spec(catalog.path().join("agent.kdl"));
        let first = prepare_launch_attempt(
            catalog.path(),
            "host-a",
            "host-a.worker",
            Generation(2),
            false,
        )
        .unwrap();
        let second = prepare_launch_attempt(
            catalog.path(),
            "host-a",
            "host-a.worker",
            Generation(2),
            false,
        )
        .unwrap();
        let retained = prepare_launch_attempt(
            catalog.path(),
            "host-a",
            "host-a.worker",
            Generation(2),
            true,
        )
        .unwrap();

        assert_ne!(first.incarnation, second.incarnation);
        assert_eq!(retained.incarnation, second.incarnation);
        inject_resume_fence(&mut spec, Generation(2), &second.incarnation).unwrap();
        let argv = spec.tasks[0].argv.as_ref().unwrap();
        assert!(
            argv.windows(2)
                .any(|pair| { pair[0] == "--required-resume-generation" && pair[1] == "2" })
        );
        assert!(argv.windows(2).any(|pair| {
            pair[0] == "--required-resume-incarnation" && pair[1] == second.incarnation
        }));
    }

    #[test]
    fn stop_failures_retry_without_refusing_and_attempt_the_complete_group() {
        let catalog = tempfile::tempdir().unwrap();
        let agent_dir = catalog.path().join("agents/host-a/worker");
        std::fs::create_dir_all(agent_dir.join("inbox")).unwrap();
        let mut spec = on_demand_spec(agent_dir.join("agent.kdl"));
        let agent_id = spec.effective_id("host-a");
        let path = crate::residency::ledger_path(catalog.path(), "host-a", &agent_id);
        let mut ledger =
            Ledger::active(&agent_id, "host-a", SessionDriver::Omp, Generation(1)).unwrap();
        ledger.apply(Event::IdleConfirmed {
            generation: Generation(1),
        });
        ledger.apply(Event::CheckpointStored {
            source: Generation(1),
            resume: Generation(2),
        });
        crate::residency::store(&path, &ledger).unwrap();
        let ids = task_ids(&spec, "host-a");
        let sessions = ids.iter().map(|id| session(id, true)).collect::<Vec<_>>();
        let runner = FailingKillRunner {
            killed: std::cell::RefCell::new(Vec::new()),
        };
        let mut report = UpReport::default();
        let debounce = crate::run::LivenessDebounce::new(Duration::from_secs(10));
        let now = Instant::now();

        before_reconcile(
            catalog.path(),
            "host-a",
            std::slice::from_mut(&mut spec),
            &sessions,
            &debounce,
            now,
            &runner,
            HostPolicy {
                idle_after: Duration::ZERO,
                warm_capacity: 0,
            },
            &mut report,
        );

        assert_eq!(&*runner.killed.borrow(), &ids);
        assert!(
            spec.tasks
                .iter()
                .all(|task| task.lifecycle == TaskLifecycle::AdoptOnly)
        );
        let stored = crate::residency::load(&path, &agent_id, "host-a", SessionDriver::Omp)
            .unwrap()
            .unwrap();
        assert!(matches!(
            stored.runtime_residency(),
            RuntimeResidency::Stopping {
                step: crate::residency::StopStep::StopOwnedGroup,
                ..
            }
        ));
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("injected kill failure"))
        );
    }

    #[test]
    fn verify_absence_waits_for_liveness_debounce() {
        let catalog = tempfile::tempdir().unwrap();
        let agent_dir = catalog.path().join("agents/host-a/worker");
        std::fs::create_dir_all(agent_dir.join("inbox")).unwrap();
        let mut spec = on_demand_spec(agent_dir.join("agent.kdl"));
        let agent_id = spec.effective_id("host-a");
        let path = crate::residency::ledger_path(catalog.path(), "host-a", &agent_id);
        let mut ledger =
            Ledger::active(&agent_id, "host-a", SessionDriver::Omp, Generation(1)).unwrap();
        ledger.apply(Event::IdleConfirmed {
            generation: Generation(1),
        });
        ledger.apply(Event::CheckpointStored {
            source: Generation(1),
            resume: Generation(2),
        });
        ledger.apply(Event::OwnedGroupStopped {
            generation: Generation(1),
        });
        crate::residency::store(&path, &ledger).unwrap();
        let ids = task_ids(&spec, "host-a");
        let alive = ids.iter().map(|id| session(id, true)).collect::<Vec<_>>();
        let runner = FailingKillRunner {
            killed: std::cell::RefCell::new(Vec::new()),
        };
        let mut report = UpReport::default();
        let mut debounce = crate::run::LivenessDebounce::new(Duration::from_secs(10));
        let observed_alive_at = Instant::now();
        debounce.observe(&alive, observed_alive_at);

        before_reconcile(
            catalog.path(),
            "host-a",
            std::slice::from_mut(&mut spec),
            &[],
            &debounce,
            observed_alive_at,
            &runner,
            HostPolicy {
                idle_after: Duration::ZERO,
                warm_capacity: 0,
            },
            &mut report,
        );

        let stored = crate::residency::load(&path, &agent_id, "host-a", SessionDriver::Omp)
            .unwrap()
            .unwrap();
        assert!(matches!(
            stored.runtime_residency(),
            RuntimeResidency::Stopping {
                step: crate::residency::StopStep::VerifyAbsent,
                ..
            }
        ));

        let confirmed_absent_at = observed_alive_at + Duration::from_secs(10);
        debounce.observe(&[], confirmed_absent_at);
        before_reconcile(
            catalog.path(),
            "host-a",
            std::slice::from_mut(&mut spec),
            &[],
            &debounce,
            confirmed_absent_at,
            &runner,
            HostPolicy {
                idle_after: Duration::ZERO,
                warm_capacity: 0,
            },
            &mut report,
        );

        let stored = crate::residency::load(&path, &agent_id, "host-a", SessionDriver::Omp)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.runtime_residency(),
            &RuntimeResidency::Cold {
                resume: Generation(2)
            }
        );
        assert!(report.errors.is_empty());
    }

    #[test]
    fn failed_persistence_does_not_advance_the_in_memory_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("not-a-directory");
        std::fs::write(&blocker, "block").unwrap();
        let mut ledger =
            Ledger::active("agent-a", "host-a", SessionDriver::Omp, Generation(1)).unwrap();

        assert!(
            store_event(
                &blocker.join("ledger.json"),
                &mut ledger,
                Event::IdleConfirmed {
                    generation: Generation(1),
                },
            )
            .is_err()
        );
        assert_eq!(ledger.runtime_residency(), &RuntimeResidency::Active);
    }

    #[test]
    fn wake_requests_are_scoped_to_exact_host_and_agent_ownership() {
        let catalog = tempfile::tempdir().unwrap();
        let path = request_wake(catalog.path(), "host-a", "agent-a").unwrap();

        assert!(has_wake_request(catalog.path(), "host-a", "agent-a").unwrap());
        assert!(!has_wake_request(catalog.path(), "host-b", "agent-a").unwrap());
        assert!(!has_wake_request(catalog.path(), "host-a", "agent-b").unwrap());
        assert!(!path.to_string_lossy().contains("agent-a"));
    }

    #[test]
    fn malformed_wake_requests_fail_closed() {
        let catalog = tempfile::tempdir().unwrap();
        let path = request_wake(catalog.path(), "host-a", "agent-a").unwrap();
        std::fs::write(path, br#"{"schema":"wrong"}"#).unwrap();

        assert!(has_wake_request(catalog.path(), "host-a", "agent-a").is_err());
    }

    #[test]
    fn group_inventory_requires_every_task_for_presence_and_none_for_absence() {
        let ids = vec!["agent".to_owned(), "sidecar".to_owned()];
        let partial = vec![session("agent", true), session("sidecar", false)];
        let complete = vec![session("agent", true), session("sidecar", true)];

        assert!(!group_present(&ids, &partial));
        assert!(!group_absent(&ids, &partial));
        assert!(group_present(&ids, &complete));
        assert!(!group_absent(&ids, &complete));
        assert!(group_absent(&ids, &[]));
    }
}
