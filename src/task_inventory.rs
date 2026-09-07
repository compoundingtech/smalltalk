//! Typed, read-only desired-task/runtime inventory.
//!
//! This is a diagnostic and automation boundary. It deliberately does not
//! expose a reconcile plan and cannot mutate catalog or runtime state.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "linux")]
use std::fs;
use std::path::{Path, PathBuf};

use agent_spec::spec::{TaskKind, TaskLifecycle};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::Discovered;
use crate::park::{ParkObserver, ParkState};

pub const TASK_INVENTORY_SCHEMA: &str = "st2.task-inventory.v2";

/// Opaque identity over one backend's stable process-generation evidence.
pub(crate) fn generation_id(
    backend: &str,
    runtime_id: &str,
    pid: u32,
    created_at: &str,
    start_time_ticks: Option<u64>,
) -> String {
    let mut hash = Sha256::new();
    let pid = pid.to_be_bytes();
    let start_time_ticks = start_time_ticks.unwrap_or(0).to_be_bytes();
    for value in [
        backend.as_bytes(),
        runtime_id.as_bytes(),
        pid.as_slice(),
        created_at.as_bytes(),
        start_time_ticks.as_slice(),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }
    format!("sha256:{:x}", hash.finalize())
}

pub(crate) fn is_rfc3339_utc_millis(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return false;
    }
    let digit_ranges = [0..4, 5..7, 8..10, 11..13, 14..16, 17..19, 20..23];
    if digit_ranges
        .into_iter()
        .flatten()
        .any(|index| !bytes[index].is_ascii_digit())
    {
        return false;
    }
    let number = |range: std::ops::Range<usize>| value[range].parse::<u32>().unwrap_or(u32::MAX);
    let month = number(5..7);
    let day = number(8..10);
    let year = number(0..4);
    let hour = number(11..13);
    let minute = number(14..16);
    let second = number(17..19);
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    year > 0 && (1..=days).contains(&day) && hour <= 23 && minute <= 59 && second <= 60
}

/// One sampling locator proven against the observed process generation.
///
/// These are observation locators, not task identity. A caller must rediscover
/// them on every inventory sample rather than retaining them as ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ResourceTarget {
    LinuxCgroupV2 {
        path: String,
    },
    DarwinProcessTree {
        #[serde(rename = "rootPid")]
        root_pid: u32,
    },
    Unavailable {
        reason: ResourceTargetUnavailableReason,
    },
}

impl ResourceTarget {
    pub(crate) fn unavailable(reason: ResourceTargetUnavailableReason) -> Self {
        Self::Unavailable { reason }
    }
}

/// Closed reasons why no truthful resource locator was available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceTargetUnavailableReason {
    NotRunning,
    RuntimeIndeterminate,
    ProcessUnavailable,
    GenerationChanged,
    CgroupV2Unavailable,
    UnsupportedPlatform,
}

/// One positively identified live runtime generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeGeneration {
    pid: u32,
    /// Backend-issued or conservatively derived RFC3339 UTC creation time.
    created_at: String,
    /// Opaque generation identity derived from stable backend evidence.
    generation_id: String,
    resource_target: ResourceTarget,
}

impl RuntimeGeneration {
    pub fn new(
        pid: u32,
        created_at: String,
        generation_id: String,
        resource_target: ResourceTarget,
    ) -> Result<Self, String> {
        if pid == 0 {
            return Err("runtime pid must be positive".into());
        }
        if !is_rfc3339_utc_millis(&created_at) {
            return Err(
                "runtime createdAt is not a valid RFC3339 UTC millisecond timestamp".into(),
            );
        }
        if generation_id.is_empty() {
            return Err("runtime generationId is empty".into());
        }
        Ok(Self {
            pid,
            created_at,
            generation_id,
            resource_target,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    pub fn generation_id(&self) -> &str {
        &self.generation_id
    }

    pub fn resource_target(&self) -> &ResourceTarget {
        &self.resource_target
    }
}

/// Fence a process-derived locator between two reads of the kernel's process
/// start token. This rejects exit and PID reuse during observation, including
/// races that happen while the cgroup membership itself is being read.
fn fence_resource_target<ReadStart, ReadTarget>(
    expected_start_time_ticks: Option<u64>,
    mut read_start: ReadStart,
    read_target: ReadTarget,
) -> ResourceTarget
where
    ReadStart: FnMut() -> Result<u64, ()>,
    ReadTarget: FnOnce() -> Result<ResourceTarget, ResourceTargetUnavailableReason>,
{
    let Ok(initial_start) = read_start() else {
        return ResourceTarget::unavailable(ResourceTargetUnavailableReason::ProcessUnavailable);
    };
    if expected_start_time_ticks.is_some_and(|expected| expected != initial_start) {
        return ResourceTarget::unavailable(ResourceTargetUnavailableReason::GenerationChanged);
    }

    let candidate = read_target();
    let Ok(final_start) = read_start() else {
        return ResourceTarget::unavailable(ResourceTargetUnavailableReason::ProcessUnavailable);
    };
    if final_start != initial_start
        || expected_start_time_ticks.is_some_and(|expected| expected != final_start)
    {
        return ResourceTarget::unavailable(ResourceTargetUnavailableReason::GenerationChanged);
    }
    candidate.unwrap_or_else(ResourceTarget::unavailable)
}

/// Parse the unified hierarchy entry without treating a legacy/hybrid entry as
/// cgroup v2 and without admitting a path that can escape the cgroup mount.
#[cfg(any(target_os = "linux", test))]
fn parse_unified_cgroup_path(raw: &str) -> Result<String, ()> {
    let mut unified = None;
    for line in raw.split_terminator('\n') {
        if line.is_empty() || line.contains('\r') || line.contains('\0') {
            return Err(());
        }
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next().ok_or(())?;
        let controllers = fields.next().ok_or(())?;
        let path = fields.next().ok_or(())?;
        if hierarchy != "0" || !controllers.is_empty() {
            continue;
        }
        if unified.is_some()
            || !path.starts_with('/')
            || (path.len() > 1 && path.ends_with('/'))
            || (path != "/"
                && path
                    .split('/')
                    .skip(1)
                    .any(|component| component.is_empty() || component == "." || component == ".."))
        {
            return Err(());
        }
        unified = Some(path.to_owned());
    }
    unified.ok_or(())
}

#[cfg(target_os = "linux")]
pub(crate) fn observe_resource_target(
    pid: u32,
    expected_start_time_ticks: Option<u64>,
) -> ResourceTarget {
    fence_resource_target(
        expected_start_time_ticks,
        || crate::exec_backend::process_start_time_ticks(pid as i32).map_err(|_| ()),
        || {
            let raw = fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .map_err(|_| ResourceTargetUnavailableReason::CgroupV2Unavailable)?;
            parse_unified_cgroup_path(&raw)
                .map(|path| ResourceTarget::LinuxCgroupV2 { path })
                .map_err(|()| ResourceTargetUnavailableReason::CgroupV2Unavailable)
        },
    )
}

#[cfg(target_os = "macos")]
pub(crate) fn observe_resource_target(
    pid: u32,
    expected_start_time_ticks: Option<u64>,
) -> ResourceTarget {
    fence_resource_target(
        expected_start_time_ticks,
        || crate::exec_backend::process_start_time_ticks(pid as i32).map_err(|_| ()),
        || Ok(ResourceTarget::DarwinProcessTree { root_pid: pid }),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn observe_resource_target(
    _pid: u32,
    _expected_start_time_ticks: Option<u64>,
) -> ResourceTarget {
    ResourceTarget::unavailable(ResourceTargetUnavailableReason::UnsupportedPlatform)
}

/// Closed runtime observation. Invalid state/generation/error products are unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedState {
    Running(RuntimeGeneration),
    Exited,
    Vanished,
    Absent,
    Indeterminate(String),
}

/// One backend observation keyed by the exact declared runtime id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeObservation {
    pub runtime_id: String,
    pub state: ObservedState,
}

/// One coherent backend observation attempt. Any backend uncertainty makes the
/// batch incomplete; callers must not infer absence from its missing rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationBatch {
    pub complete: bool,
    pub observations: Vec<RuntimeObservation>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRuntime {
    pub runtime_id: String,
    pub kind: TaskKind,
}

/// Read-only backend boundary used by the CLI and deterministic tests.
pub trait RuntimeObserver {
    fn observe(&self, desired: &[DesiredRuntime]) -> ObservationBatch;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInventory {
    schema: &'static str,
    catalog: PathBuf,
    host: String,
    complete: bool,
    errors: Vec<String>,
    tasks: Vec<TaskRow>,
}

impl TaskInventory {
    pub fn incomplete(catalog: PathBuf, host: String, error: String) -> Self {
        Self {
            schema: TASK_INVENTORY_SCHEMA,
            catalog,
            host,
            complete: false,
            errors: vec![error],
            tasks: Vec::new(),
        }
    }

    pub fn complete(&self) -> bool {
        self.complete
    }

    pub fn mark_incomplete(&mut self, error: impl Into<String>) {
        push_error(&mut self.errors, error.into());
        self.complete = false;
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("task inventory contains only serializable values")
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskRow {
    agent: String,
    task: String,
    runtime_id: String,
    kind: &'static str,
    lifecycle: &'static str,
    retired: bool,
    desired_state: &'static str,
    agent_desired_state: String,
    agent_desired_state_reason: Option<String>,
    runtime: RuntimeJson,
    /// The supervisor's park decision, when it has parked this task. Deliberately its own field
    /// rather than a `runtime.state` variant: `runtime` is a closed *observation* of the process, and
    /// the park path keeps the corpse on purpose, so whether a parked task reads `exited` or `absent`
    /// is real evidence that a `"parked"` state would have overwritten.
    parked: Option<ParkedJson>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ParkedJson {
    since: String,
    reason: String,
    supervisor_pid: u32,
    /// What actually clears it. The whole complaint in #204 is that an operator could see a task that
    /// should be up, is not up, and had nothing visibly wrong with it — so the remedy travels with
    /// the fault rather than living in a journal line nobody knew to grep for.
    recovery: RecoveryAction,
}

#[derive(Debug, Serialize)]
struct RecoveryAction {
    argv: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeJson {
    state: &'static str,
    pid: Option<u32>,
    created_at: Option<String>,
    generation_id: Option<String>,
    resource_target: ResourceTarget,
    error: Option<String>,
}

#[derive(Debug)]
struct DesiredTask {
    agent: String,
    task: String,
    runtime_id: String,
    kind: TaskKind,
    lifecycle: TaskLifecycle,
    retired: bool,
    agent_desired_state: String,
    agent_desired_state_reason: Option<String>,
}

/// Whether two deterministic discovery passes describe the same semantic catalog.
///
/// This detects observed declaration drift without claiming writer serialization.
/// A change that normalizes to the same declaration is intentionally equivalent.
pub fn same_discovery(left: &Discovered, right: &Discovered) -> bool {
    left.specs == right.specs && left.warnings == right.warnings && left.errors == right.errors
}

/// Join one discovered declaration observation to one runtime observation.
///
/// `catalog` must already be canonicalized. The caller is responsible for
/// checking that declaration semantics did not drift across runtime observation.
pub fn inventory(
    catalog: &Path,
    host: &str,
    found: &Discovered,
    observer: &dyn RuntimeObserver,
    parks: &dyn ParkObserver,
) -> TaskInventory {
    let mut errors = found
        .errors
        .iter()
        .map(|error| format!("catalog file {}: {}", error.path.display(), error.message))
        .collect::<Vec<_>>();
    let mut desired = Vec::new();
    let mut runtime_owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut compiled_specs = found.specs.clone();
    let compilation = crate::reconcile::TaskCompileContext::current(catalog.to_path_buf())
        .and_then(|context| {
            crate::reconcile::compile_generated_tasks(&mut compiled_specs, host, &context)
        });
    if let Err(error) = compilation {
        push_error(&mut errors, format!("compile desired tasks: {error:#}"));
        compiled_specs.clear();
    }

    for spec in &compiled_specs {
        if spec.resolved_host(host) != host {
            continue;
        }
        let bus_id = spec.bus_id(host);
        for task in &spec.tasks {
            // Active declaration-only metadata has no desired runtime. Retired tasks remain in the
            // inventory even without launch material so stale generations stay visible.
            if spec.desired_state.is_running() && task.command.is_none() && task.argv.is_none() {
                continue;
            }
            let runtime_id = task
                .id
                .clone()
                .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
            runtime_owners
                .entry(runtime_id.clone())
                .or_default()
                .push(format!("{bus_id}/{}", task.name));
            desired.push(DesiredTask {
                agent: bus_id.clone(),
                task: task.name.clone(),
                runtime_id,
                kind: task.kind,
                lifecycle: task.lifecycle,
                retired: spec.desired_state.is_retired(),
                agent_desired_state: spec.desired_state.as_str().to_owned(),
                agent_desired_state_reason: spec.desired_state.reason().map(str::to_owned),
            });
        }
    }

    for (runtime_id, owners) in &runtime_owners {
        if owners.len() > 1 {
            errors.push(format!(
                "duplicate runtime id {runtime_id:?} is declared by {}",
                owners.join(", ")
            ));
        }
    }

    let desired_runtimes = desired
        .iter()
        .map(|task| DesiredRuntime {
            runtime_id: task.runtime_id.clone(),
            kind: task.kind,
        })
        .collect::<Vec<_>>();
    let desired_ids = desired_runtimes
        .iter()
        .map(|runtime| runtime.runtime_id.as_str())
        .collect::<BTreeSet<_>>();
    let observed = observer.observe(&desired_runtimes);
    for error in &observed.errors {
        push_error(&mut errors, error.clone());
    }
    if !observed.complete && observed.errors.is_empty() {
        push_error(
            &mut errors,
            "runtime observer reported an incomplete batch".into(),
        );
    }
    let mut by_id: BTreeMap<String, RuntimeObservation> = BTreeMap::new();
    let mut duplicate_observations = BTreeSet::new();
    for observation in observed.observations {
        if !desired_ids.contains(observation.runtime_id.as_str()) {
            push_error(
                &mut errors,
                format!(
                    "runtime observer returned undeclared id {:?}",
                    observation.runtime_id
                ),
            );
            continue;
        }
        if let ObservedState::Indeterminate(error) = &observation.state {
            push_error(&mut errors, error.clone());
        }
        let runtime_id = observation.runtime_id.clone();
        if by_id.insert(runtime_id.clone(), observation).is_some() {
            duplicate_observations.insert(runtime_id);
        }
    }
    for runtime_id in duplicate_observations {
        errors.push(format!(
            "runtime observer returned duplicate id {runtime_id:?}"
        ));
        by_id.insert(
            runtime_id.clone(),
            RuntimeObservation {
                runtime_id,
                state: ObservedState::Indeterminate("duplicate runtime observation".into()),
            },
        );
    }

    let observation_complete = observed.complete && observed.errors.is_empty();

    // The park projection is joined on the same declared ids. A parked task is a *known* fault, so it
    // never makes the envelope incomplete — only a marker that cannot be believed does.
    let mut parks = parks.observe(
        &desired_runtimes
            .iter()
            .map(|runtime| runtime.runtime_id.clone())
            .collect::<Vec<_>>(),
    );
    for error in &parks.errors {
        push_error(&mut errors, error.clone());
    }
    if !parks.complete && parks.errors.is_empty() {
        push_error(
            &mut errors,
            "park projection reported an incomplete batch".into(),
        );
    }

    desired.sort_by(|a, b| {
        (&a.agent, &a.task, &a.runtime_id).cmp(&(&b.agent, &b.task, &b.runtime_id))
    });
    let tasks = desired
        .into_iter()
        .map(|task| {
            let observation = by_id.remove(&task.runtime_id).unwrap_or_else(|| {
                if observation_complete {
                    RuntimeObservation {
                        runtime_id: task.runtime_id.clone(),
                        state: ObservedState::Absent,
                    }
                } else {
                    RuntimeObservation {
                        runtime_id: task.runtime_id.clone(),
                        state: ObservedState::Indeterminate(
                            "runtime observation incomplete".into(),
                        ),
                    }
                }
            });
            let (state, pid, created_at, generation_id, resource_target, error) =
                match observation.state {
                    ObservedState::Running(generation) => (
                        "running",
                        Some(generation.pid),
                        Some(generation.created_at),
                        Some(generation.generation_id),
                        generation.resource_target,
                        None,
                    ),
                    ObservedState::Exited => (
                        "exited",
                        None,
                        None,
                        None,
                        ResourceTarget::unavailable(ResourceTargetUnavailableReason::NotRunning),
                        None,
                    ),
                    ObservedState::Vanished => (
                        "vanished",
                        None,
                        None,
                        None,
                        ResourceTarget::unavailable(ResourceTargetUnavailableReason::NotRunning),
                        None,
                    ),
                    ObservedState::Absent => (
                        "absent",
                        None,
                        None,
                        None,
                        ResourceTarget::unavailable(ResourceTargetUnavailableReason::NotRunning),
                        None,
                    ),
                    ObservedState::Indeterminate(error) => (
                        "indeterminate",
                        None,
                        None,
                        None,
                        ResourceTarget::unavailable(
                            ResourceTargetUnavailableReason::RuntimeIndeterminate,
                        ),
                        Some(error),
                    ),
                };
            let parked = match parks.states.remove(&task.runtime_id) {
                Some(ParkState::Parked(record)) => Some(ParkedJson {
                    since: record.parked_at,
                    reason: record.reason,
                    supervisor_pid: record.supervisor_pid,
                    recovery: RecoveryAction {
                        argv: vec![
                            "st2".to_string(),
                            "--catalog".to_string(),
                            catalog.display().to_string(),
                            "unpark".to_string(),
                            task.runtime_id.clone(),
                            "--host".to_string(),
                            host.to_string(),
                        ],
                    },
                }),
                _ => None,
            };
            TaskRow {
                agent: task.agent,
                task: task.task,
                runtime_id: task.runtime_id,
                kind: match task.kind {
                    TaskKind::Pty => "pty",
                    TaskKind::Exec => "exec",
                },
                lifecycle: match task.lifecycle {
                    TaskLifecycle::Service => "service",
                    TaskLifecycle::AdoptOnly => "adopt-only",
                },
                retired: task.retired,
                desired_state: if task.agent_desired_state == "running" {
                    "running"
                } else {
                    "absent"
                },
                agent_desired_state: task.agent_desired_state,
                agent_desired_state_reason: task.agent_desired_state_reason,
                runtime: RuntimeJson {
                    state,
                    pid,
                    created_at,
                    generation_id,
                    resource_target,
                    error,
                },
                parked,
            }
        })
        .collect();

    TaskInventory {
        schema: TASK_INVENTORY_SCHEMA,
        catalog: catalog.to_path_buf(),
        host: host.to_owned(),
        complete: errors.is_empty() && observation_complete,
        errors,
        tasks,
    }
}

fn push_error(errors: &mut Vec<String>, error: String) {
    if !errors.contains(&error) {
        errors.push(error);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;

    #[derive(Clone)]
    struct FixedObserver(ObservationBatch);

    impl RuntimeObserver for FixedObserver {
        fn observe(&self, _desired: &[DesiredRuntime]) -> ObservationBatch {
            self.0.clone()
        }
    }

    fn write_agent(catalog: &Path, host: &str, identity: &str, body: &str) {
        let dir = catalog.join("agents").join(host).join(identity);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("agent.kdl"),
            format!("agent \"{identity}\" {{ host \"{host}\"; {body} }}\n"),
        )
        .unwrap();
    }

    #[derive(Clone, Default)]
    struct FixedParks(crate::park::ParkBatch);

    impl ParkObserver for FixedParks {
        fn observe(&self, _desired: &[String]) -> crate::park::ParkBatch {
            self.0.clone()
        }
    }

    fn json(catalog: &Path, host: &str, observer: ObservationBatch) -> serde_json::Value {
        json_with_parks(catalog, host, observer, crate::park::NoParks)
    }

    fn json_with_parks(
        catalog: &Path,
        host: &str,
        observer: ObservationBatch,
        parks: impl ParkObserver,
    ) -> serde_json::Value {
        let found = crate::discover(catalog);
        serde_json::from_str(
            &inventory(catalog, host, &found, &FixedObserver(observer), &parks).to_json(),
        )
        .unwrap()
    }

    fn park_batch(complete: bool, states: &[(&str, ParkState)], errors: &[&str]) -> FixedParks {
        FixedParks(crate::park::ParkBatch {
            complete,
            states: states
                .iter()
                .map(|(id, state)| ((*id).to_string(), state.clone()))
                .collect(),
            errors: errors.iter().map(|error| (*error).to_string()).collect(),
        })
    }

    fn park_record(runtime_id: &str) -> ParkState {
        ParkState::Parked(crate::park::ParkRecord {
            schema: crate::park::PARK_SCHEMA.to_string(),
            runtime_id: runtime_id.to_string(),
            supervisor_pid: 4242,
            supervisor_start_time_ticks: 99,
            parked_at: "2026-08-09T10:00:00.000Z".into(),
            reason: "crash-looped past its restart{} policy (mode=fail)".into(),
        })
    }

    fn running(id: &str, pid: u32) -> RuntimeObservation {
        running_with_target(
            id,
            pid,
            ResourceTarget::unavailable(ResourceTargetUnavailableReason::ProcessUnavailable),
        )
    }

    fn running_with_target(
        id: &str,
        pid: u32,
        resource_target: ResourceTarget,
    ) -> RuntimeObservation {
        RuntimeObservation {
            runtime_id: id.into(),
            state: ObservedState::Running(
                RuntimeGeneration::new(
                    pid,
                    "2026-07-31T10:00:00.000Z".into(),
                    format!("sha256:g-{pid}"),
                    resource_target,
                )
                .unwrap(),
            ),
        }
    }

    #[test]
    fn stable_wire_shape_maps_pty_exec_explicit_default_and_ignores_foreign_host() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"
              pty "agent" {
                id "h.worker"
                lifecycle "adopt-only"
                argv "agent-bin"
              }
              exec "ding" { argv "st2" "ding" }
            "#,
        );
        write_agent(
            tmp.path(),
            "other",
            "foreign",
            r#"pty "agent" { argv "must-not-appear" }"#,
        );
        let value = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![running("h.worker", 11), running("h.worker.ding", 12)],
                errors: vec![],
            },
        );
        assert_eq!(value["schema"], TASK_INVENTORY_SCHEMA);
        assert_eq!(value["catalog"], tmp.path().to_str().unwrap());
        assert_eq!(value["host"], "h");
        assert_eq!(value["complete"], true);
        assert_eq!(value["errors"], Value::Array(vec![]));
        assert_eq!(value["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["tasks"][0],
            serde_json::json!({
                "agent": "h.worker",
                "task": "agent",
                "runtimeId": "h.worker",
                "kind": "pty",
                "lifecycle": "adopt-only",
                "retired": false,
                "desiredState": "running",
                "agentDesiredState": "running",
                "agentDesiredStateReason": null,
                "runtime": {
                    "state": "running",
                    "pid": 11,
                    "createdAt": "2026-07-31T10:00:00.000Z",
                    "generationId": "sha256:g-11",
                    "resourceTarget": {
                        "type": "unavailable",
                        "reason": "processUnavailable"
                    },
                    "error": null
                },
                "parked": null
            })
        );
        assert_eq!(value["tasks"][1]["runtimeId"], "h.worker.ding");
        assert_eq!(value["tasks"][1]["kind"], "exec");
        assert_eq!(value["tasks"][1]["lifecycle"], "service");
        assert!(
            value["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["agent"] != "other.foreign")
        );
    }

    /// The whole operational complaint in #204: a parked task reported `desiredState: running`,
    /// nothing running, and `error: null`, so an operator saw a task that should be up, was not up,
    /// and had nothing visibly wrong with it. The only record of the fault was a supervisor journal
    /// line you had to already know to grep for.
    ///
    /// The park rides alongside a *truthful* runtime observation rather than replacing it. The park
    /// path keeps the dead session on purpose ("leaving it parked and its last session for
    /// inspection"), so `exited` versus `absent` distinguishes a corpse left as evidence from a task
    /// that never got that far — evidence a `"parked"` runtime state would have destroyed.
    #[test]
    fn a_parked_task_reports_its_fault_alongside_a_truthful_runtime_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "flapper",
            r#"pty "agent" { id "h.flapper"; argv "agent-bin" }"#,
        );
        write_agent(
            tmp.path(),
            "h",
            "healthy",
            r#"pty "agent" { id "h.healthy"; argv "agent-bin" }"#,
        );

        let value = json_with_parks(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![
                    RuntimeObservation {
                        runtime_id: "h.flapper".into(),
                        state: ObservedState::Exited,
                    },
                    running("h.healthy", 77),
                ],
                errors: vec![],
            },
            park_batch(
                true,
                &[
                    ("h.flapper", park_record("h.flapper")),
                    ("h.healthy", ParkState::NotParked),
                ],
                &[],
            ),
        );

        let flapper = &value["tasks"][0];
        assert_eq!(flapper["runtimeId"], "h.flapper");
        assert_eq!(flapper["desiredState"], "running");
        assert_eq!(
            flapper["runtime"]["state"], "exited",
            "the retained corpse is evidence; the park must not overwrite it"
        );
        assert_eq!(flapper["runtime"]["error"], Value::Null);
        assert_eq!(
            flapper["parked"],
            serde_json::json!({
                "since": "2026-08-09T10:00:00.000Z",
                "reason": "crash-looped past its restart{} policy (mode=fail)",
                "supervisorPid": 4242,
                "recovery": {
                    "argv": [
                        "st2",
                        "--catalog",
                        tmp.path().display().to_string(),
                        "unpark",
                        "h.flapper",
                        "--host",
                        "h"
                    ]
                }
            }),
            "desiredState=running + nothing running + error=null is still the only visible signal"
        );

        assert_eq!(value["tasks"][1]["runtimeId"], "h.healthy");
        assert_eq!(value["tasks"][1]["parked"], Value::Null);

        // A park is a KNOWN fault. Making it incomplete would conflate "st2 decided to stop
        // restarting this" with "st2 could not tell what is going on", and would make `st2 tasks`
        // exit non-zero for the entire time a crash-looper sits parked.
        assert_eq!(value["complete"], true);
        assert_eq!(value["errors"], Value::Array(vec![]));
    }

    /// A marker that cannot be believed is different from a task that is not parked. Unreadable
    /// evidence follows the rest of this surface and fails closed.
    #[test]
    fn an_unbelievable_park_marker_makes_the_envelope_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"pty "agent" { id "h.worker"; argv "agent-bin" }"#,
        );

        let value = json_with_parks(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![running("h.worker", 11)],
                errors: vec![],
            },
            park_batch(
                false,
                &[(
                    "h.worker",
                    ParkState::Indeterminate("park marker for \"h.worker\": bad".into()),
                )],
                &["park marker for \"h.worker\": bad"],
            ),
        );

        assert_eq!(value["complete"], false);
        assert_eq!(value["errors"][0], "park marker for \"h.worker\": bad");
        assert_eq!(
            value["tasks"][0]["parked"],
            Value::Null,
            "an unreadable marker must not be reported as a confirmed park"
        );
    }

    #[test]
    fn complete_missing_runtime_is_absent_but_incomplete_is_indeterminate() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"pty "agent" { id "h.worker"; argv "agent-bin" }"#,
        );

        let absent = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![],
                errors: vec![],
            },
        );
        assert_eq!(absent["complete"], true);
        assert_eq!(absent["tasks"][0]["runtime"]["state"], "absent");

        let indeterminate = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: false,
                observations: vec![],
                errors: vec!["pty timed out".into()],
            },
        );
        assert_eq!(indeterminate["complete"], false);
        assert_eq!(
            indeterminate["tasks"][0]["runtime"]["state"],
            "indeterminate"
        );
        assert_eq!(
            indeterminate["tasks"][0]["runtime"]["error"],
            "runtime observation incomplete"
        );
    }

    #[test]
    fn retired_task_desires_absence_without_erasing_observed_generation() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "old",
            r#"retired #true; pty "agent" { id "h.old" }"#,
        );
        let value = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![running("h.old", 42)],
                errors: vec![],
            },
        );
        assert_eq!(value["tasks"][0]["retired"], true);
        assert_eq!(value["tasks"][0]["desiredState"], "absent");
        assert_eq!(value["tasks"][0]["runtime"]["state"], "running");
        assert_eq!(value["tasks"][0]["runtime"]["pid"], 42);
    }

    #[test]
    fn duplicate_observation_is_indeterminate_and_non_complete() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"pty "agent" { id "h.worker"; argv "agent-bin" }"#,
        );
        let value = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![running("h.worker", 1), running("h.worker", 2)],
                errors: vec![],
            },
        );
        assert_eq!(value["complete"], false);
        assert_eq!(value["tasks"][0]["runtime"]["state"], "indeterminate");
        assert!(
            value["errors"][0]
                .as_str()
                .unwrap()
                .contains("duplicate id")
        );
    }

    #[test]
    fn invalid_running_evidence_and_timestamp_fail_closed() {
        assert!(!is_rfc3339_utc_millis("2026-02-31T10:00:00.000Z"));
        assert!(!is_rfc3339_utc_millis("2025-02-29T10:00:00.000Z"));
        assert!(is_rfc3339_utc_millis("2024-02-29T10:00:00.000Z"));
        assert!(
            RuntimeGeneration::new(
                0,
                "2026-07-31T10:00:00.000Z".into(),
                "sha256:g".into(),
                ResourceTarget::unavailable(ResourceTargetUnavailableReason::ProcessUnavailable),
            )
            .is_err()
        );

        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"pty "agent" { argv "agent-bin" }"#,
        );
        let value = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: true,
                observations: vec![RuntimeObservation {
                    runtime_id: "h.worker.agent".into(),
                    state: ObservedState::Indeterminate(
                        "running runtime lacks complete generation evidence".into(),
                    ),
                }],
                errors: vec![],
            },
        );
        assert_eq!(value["complete"], false);
        assert_eq!(value["tasks"][0]["runtime"]["state"], "indeterminate");
    }

    #[test]
    fn closed_runtime_states_serialize_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        for identity in ["a", "b", "c"] {
            write_agent(
                tmp.path(),
                "h",
                identity,
                &format!(r#"pty "agent" {{ id "h.{identity}"; argv "x" }}"#),
            );
        }
        let value = json(
            tmp.path(),
            "h",
            ObservationBatch {
                complete: false,
                observations: vec![
                    RuntimeObservation {
                        runtime_id: "h.a".into(),
                        state: ObservedState::Exited,
                    },
                    RuntimeObservation {
                        runtime_id: "h.b".into(),
                        state: ObservedState::Vanished,
                    },
                    RuntimeObservation {
                        runtime_id: "h.c".into(),
                        state: ObservedState::Indeterminate("unreadable".into()),
                    },
                ],
                errors: vec!["one runtime unreadable".into()],
            },
        );
        assert_eq!(value["tasks"][0]["runtime"]["state"], "exited");
        assert_eq!(value["tasks"][1]["runtime"]["state"], "vanished");
        assert_eq!(value["tasks"][2]["runtime"]["state"], "indeterminate");
    }

    #[test]
    fn unified_cgroup_parser_accepts_one_exact_v2_path_and_rejects_ambiguous_or_unsafe_input() {
        assert_eq!(
            parse_unified_cgroup_path("9:cpu,cpuacct:/legacy\n0::/user.slice/st2:worker scope\n"),
            Ok("/user.slice/st2:worker scope".into())
        );
        assert_eq!(parse_unified_cgroup_path("0::/\n"), Ok("/".into()));

        for malformed in [
            "",
            "9:cpu:/legacy\n",
            "0:memory:/not-unified\n",
            "0::relative\n",
            "0::/a/../b\n",
            "0::/a//b\n",
            "0::/a/\n",
            "0::/first\n0::/second\n",
            "0::/ok\r\n",
            "0::/ok\n\n0::/also\n",
        ] {
            assert_eq!(
                parse_unified_cgroup_path(malformed),
                Err(()),
                "{malformed:?} was admitted"
            );
        }
    }

    #[test]
    fn process_fence_accepts_only_the_same_live_generation() {
        let expected = ResourceTarget::LinuxCgroupV2 {
            path: "/st2/task".into(),
        };
        let mut stable = [Ok(41), Ok(41)].into_iter();
        assert_eq!(
            fence_resource_target(Some(41), || stable.next().unwrap(), || Ok(expected.clone())),
            expected
        );

        let mut target_read = false;
        assert_eq!(
            fence_resource_target(
                Some(41),
                || Ok(42),
                || {
                    target_read = true;
                    Ok(ResourceTarget::LinuxCgroupV2 {
                        path: "/already-recycled".into(),
                    })
                }
            ),
            ResourceTarget::unavailable(ResourceTargetUnavailableReason::GenerationChanged)
        );
        assert!(!target_read, "a mismatched generation read the target");

        let mut recycled = [Ok(41), Ok(42)].into_iter();
        assert_eq!(
            fence_resource_target(
                Some(41),
                || recycled.next().unwrap(),
                || Ok(ResourceTarget::LinuxCgroupV2 {
                    path: "/wrong-generation".into(),
                })
            ),
            ResourceTarget::unavailable(ResourceTargetUnavailableReason::GenerationChanged)
        );

        let mut exited = [Ok(41), Err(())].into_iter();
        assert_eq!(
            fence_resource_target(
                Some(41),
                || exited.next().unwrap(),
                || Ok(ResourceTarget::LinuxCgroupV2 {
                    path: "/exited".into(),
                })
            ),
            ResourceTarget::unavailable(ResourceTargetUnavailableReason::ProcessUnavailable)
        );
    }

    #[test]
    fn degraded_linux_and_darwin_targets_have_strict_tagged_wire_shapes() {
        let mut stable = [Ok(7), Ok(7)].into_iter();
        let degraded = fence_resource_target(
            Some(7),
            || stable.next().unwrap(),
            || Err(ResourceTargetUnavailableReason::CgroupV2Unavailable),
        );
        assert_eq!(
            serde_json::to_value(degraded).unwrap(),
            serde_json::json!({
                "type": "unavailable",
                "reason": "cgroupV2Unavailable"
            })
        );
        assert_eq!(
            serde_json::to_value(ResourceTarget::DarwinProcessTree { root_pid: 73 }).unwrap(),
            serde_json::json!({
                "type": "darwinProcessTree",
                "rootPid": 73
            })
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn current_process_resource_target_is_associated_with_its_proven_generation() {
        let pid = std::process::id();
        let start_time_ticks = crate::exec_backend::process_start_time_ticks(pid as i32).unwrap();
        let target = observe_resource_target(pid, Some(start_time_ticks));
        #[cfg(target_os = "linux")]
        assert!(matches!(
            target,
            ResourceTarget::LinuxCgroupV2 { ref path } if path.starts_with('/')
        ));
        #[cfg(target_os = "macos")]
        assert_eq!(target, ResourceTarget::DarwinProcessTree { root_pid: pid });
    }

    #[test]
    fn semantic_discovery_drift_is_detected_but_comment_only_edits_are_equivalent() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(tmp.path(), "h", "worker", r#"pty "agent" { argv "first" }"#);
        let first = crate::discover(tmp.path());
        let path = tmp.path().join("agents/h/worker/agent.kdl");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("// comment\n{original}")).unwrap();
        let comment_only = crate::discover(tmp.path());
        assert!(same_discovery(&first, &comment_only));
        write_agent(
            tmp.path(),
            "h",
            "worker",
            r#"pty "agent" { argv "second" }"#,
        );
        let changed = crate::discover(tmp.path());
        assert!(!same_discovery(&first, &changed));
    }
}
