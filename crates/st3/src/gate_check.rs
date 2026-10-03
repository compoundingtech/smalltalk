//! `st missions check`: run each exec gate of a mission file once, now, the way a run would, and
//! report its answer.
//!
//! A check runs on the daemon, in the gate's workspace and with the environment st gives a gate
//! it runs: the captured login-shell environment, the recorder, the `st` variables and
//! `ST_GATE_REPORT`. Run variables stand for a run that does not exist yet. Gates run one at a
//! time, in the order the file declares them, as a step runs its gates one after another. A gate
//! declared for another host, or whose workspace does not exist yet, is unchecked. Checks live
//! only in this process; nothing reaches the graph.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use anyhow::{Context as _, Result};

use crate::model::{GateCheckItemView, GateCheckView, GateSpec, MissionSpec, NormalizedIntent};

/// A check stays readable this long after it starts, so a client can read its last answers.
const KEEP_MS: u128 = 60 * 60_000;

/// The daemon's own endpoint, for the `st` commands a check runs, as members get it.
static ENDPOINT: OnceLock<String> = OnceLock::new();

static CHECKS: LazyLock<Mutex<HashMap<String, Check>>> = LazyLock::new(Default::default);

/// Record the endpoint the daemon serves its local API on.
pub fn set_endpoint(endpoint: String) {
    let _ = ENDPOINT.set(endpoint);
}

/// Where a check's processes find st and its state, as a member the reconciler starts does.
pub struct CheckHost<'a> {
    pub node: &'a str,
    pub state_dir: &'a Path,
    pub pty_root: &'a Path,
}

struct Check {
    started_at: u128,
    /// Holds each gate's output and report files; removed with the check.
    directory: PathBuf,
    host: String,
    environment: BTreeMap<String, String>,
    /// The command recorder members put first on PATH, when this daemon installed it.
    recorder: Option<PathBuf>,
    items: Vec<Item>,
}

struct Item {
    view: GateCheckItemView,
    environment: BTreeMap<String, String>,
    time_limit_ms: u64,
    /// Where the check writes its output and its `st` commands their reports.
    log: PathBuf,
    report: PathBuf,
    child: Option<Child>,
    started: Option<Instant>,
}

/// Start checking every exec gate the intent's missions declare, with `workspace` standing for a
/// run's workspace and `inputs` for its input values, and return the check as it starts. A gate
/// that reads an input `inputs` does not give is unchecked.
pub fn start(
    host: CheckHost<'_>,
    intent: &NormalizedIntent,
    workspace: &str,
    inputs: &BTreeMap<String, String>,
) -> Result<GateCheckView> {
    sweep();
    let id = uuid::Uuid::now_v7().simple().to_string();
    let directory = host.state_dir.join("gate-checks").join(&id);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create the gate check directory {}", directory.display()))?;
    let mut items = Vec::new();
    for mission in intent.missions.values() {
        let mut variables = stand_in_variables(mission, workspace, &id);
        let mut missing = std::collections::BTreeSet::new();
        for name in mission.inputs.keys() {
            match inputs.get(name) {
                Some(value) => {
                    variables.insert(format!("input.{name}"), value.clone());
                }
                None => {
                    missing.insert(name.clone());
                }
            }
        }
        let scope = Scope {
            variables: &variables,
            missing_inputs: &missing,
            workspace,
        };
        collect(mission, "mission".into(), &scope, &mut items);
    }
    for (index, item) in items.iter_mut().enumerate() {
        item.log = directory.join(format!("{index}.log"));
        item.report = directory.join(format!("{index}.report"));
        if item.view.answer == "waiting" && item.view.host != host.node {
            item.view.answer = "unchecked".into();
            item.view.reason = Some(format!(
                "it runs on host `{}`; run `st missions check` there",
                item.view.host
            ));
        }
    }
    let mut check = Check {
        started_at: now_ms(),
        directory,
        host: host.node.to_owned(),
        environment: check_environment(&host)?,
        recorder: crate::recorder::directory(host.state_dir)
            .ok()
            .filter(|directory| directory.is_dir()),
        items,
    };
    advance(&mut check);
    let view = view(&id, &check);
    CHECKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, check);
    Ok(view)
}

/// The check `id` as it stands now, after starting its next gate when the previous one answered.
pub fn poll(id: &str) -> Option<GateCheckView> {
    sweep();
    let mut checks = CHECKS.lock().unwrap_or_else(PoisonError::into_inner);
    let check = checks.get_mut(id)?;
    advance(check);
    Some(view(id, check))
}

fn view(id: &str, check: &Check) -> GateCheckView {
    GateCheckView {
        id: id.to_owned(),
        host: check.host.clone(),
        finished: check.items.iter().all(|item| finished(&item.view.answer)),
        gates: check.items.iter().map(|item| item.view.clone()).collect(),
    }
}

fn finished(answer: &str) -> bool {
    !matches!(answer, "waiting" | "running")
}

/// Stop gates past their time limit in every check, and forget checks older than [`KEEP_MS`].
fn sweep() {
    let mut checks = CHECKS.lock().unwrap_or_else(PoisonError::into_inner);
    let now = now_ms();
    checks.retain(|_, check| {
        for item in &mut check.items {
            settle(item);
        }
        let keep = now.saturating_sub(check.started_at) < KEEP_MS;
        if !keep {
            for item in &mut check.items {
                stop(item);
            }
            let _ = std::fs::remove_dir_all(&check.directory);
        }
        keep
    });
}

/// Record the answer of the running gate if it has one, then start the next waiting gate.
fn advance(check: &mut Check) {
    for item in &mut check.items {
        settle(item);
        match item.view.answer.as_str() {
            "running" => return,
            "waiting" => {
                launch(item, &check.environment, check.recorder.as_deref());
                if item.view.answer == "running" {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Settle a running gate that exited or ran past its time limit.
fn settle(item: &mut Item) {
    let Some(child) = item.child.as_mut() else {
        return;
    };
    let elapsed = item
        .started
        .map_or(0, |started| started.elapsed().as_millis());
    let exit_code = match child.try_wait() {
        Ok(Some(status)) => exit_code(status),
        Ok(None) if elapsed < u128::from(item.time_limit_ms) => {
            item.view.elapsed_ms = elapsed as u64;
            return;
        }
        Ok(None) | Err(_) => {
            stop(item);
            item.child = None;
            item.view.elapsed_ms = elapsed as u64;
            finish(
                item,
                None,
                Some(format!(
                    "its check ran past its time limit of {}",
                    crate::reconcile::render_duration_ms(item.time_limit_ms)
                )),
            );
            return;
        }
    };
    item.child = None;
    item.view.elapsed_ms = elapsed as u64;
    finish(item, exit_code, None);
}

fn finish(item: &mut Item, exit_code: Option<i64>, start_failure: Option<String>) {
    let calls = crate::gate_report::read(&item.report);
    item.view.output = crate::reconcile::output_tail(
        &std::fs::read_to_string(&item.log).unwrap_or_default(),
        crate::reconcile::GATE_OUTPUT_LINES,
        crate::reconcile::GATE_OUTPUT_BYTES,
    );
    item.view.exit_code = exit_code;
    let broken = crate::reconcile::exec_check_broken(exit_code, &calls, start_failure.as_deref());
    item.view.calls = calls;
    item.view.answer = match (&broken, exit_code) {
        (Some(_), _) => "broken",
        (None, Some(0)) => "pass",
        (None, _) => "not-yet",
    }
    .into();
    item.view.reason = broken;
}

/// The exit status as the driver that wraps a gate reports it: a signal is 128 + its number.
fn exit_code(status: std::process::ExitStatus) -> Option<i64> {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .map(i64::from)
        .or_else(|| status.signal().map(|signal| 128 + i64::from(signal)))
}

fn stop(item: &mut Item) {
    if let Some(child) = item.child.as_mut() {
        if let Ok(pid) = i32::try_from(child.id()) {
            // The check leads its own process group; end everything it started.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn launch(item: &mut Item, base: &BTreeMap<String, String>, recorder: Option<&Path>) {
    let workspace = PathBuf::from(&item.view.workspace);
    if !workspace.is_dir() {
        item.view.answer = "unchecked".into();
        item.view.reason = Some(format!(
            "its workspace {} does not exist here yet",
            workspace.display()
        ));
        return;
    }
    match spawn(item, &workspace, base, recorder) {
        Ok(child) => {
            item.child = Some(child);
            item.started = Some(Instant::now());
            item.view.answer = "running".into();
        }
        Err(error) => finish(
            item,
            None,
            Some(format!("its check could not start: {error:#}")),
        ),
    }
}

fn spawn(
    item: &Item,
    workspace: &Path,
    base: &BTreeMap<String, String>,
    recorder: Option<&Path>,
) -> Result<Child> {
    let (log, report) = (&item.log, &item.report);
    let mut declared = item.environment.clone();
    for (key, value) in base {
        if key.starts_with("ST3_") {
            declared.insert(key.clone(), value.clone());
        } else {
            declared.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    // A gate acts as no agent, as the reconciler starts it.
    declared.remove("ST_AGENT");
    declared.remove(crate::suspension::RESUME_ENV);
    declared.insert(
        crate::gate_report::ENV.into(),
        report.to_string_lossy().into_owned(),
    );
    let executable = crate::reconcile::launch_executable()?;
    let environment = crate::reconcile::member_environment(&declared, &executable, recorder)?;
    let mut source = item.view.command.clone();
    st_runtime::expand_path_placeholder(&mut source, &environment);
    let shell = st_runtime::resolve_executable("sh", &environment)?;
    let output = std::fs::File::create(log)
        .with_context(|| format!("create the check log {}", log.display()))?;
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(source)
        .env_clear()
        .envs(&environment)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output);
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    Ok(command.spawn()?)
}

/// The variables every check's gates get beside their own, as `perform_start` sets them.
fn check_environment(host: &CheckHost<'_>) -> Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::from([
        (
            "PTY_ROOT".to_owned(),
            host.pty_root.to_string_lossy().into_owned(),
        ),
        (
            "ST_HOOKS".to_owned(),
            crate::hooks::set_dir(&crate::hooks::root(host.state_dir))
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "ST3_DRIVER_STATE_DIR".to_owned(),
            host.state_dir
                .join("drivers")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let link = crate::reconcile::st_binary_link(host.state_dir);
    let st_binary = if link.exists() {
        link
    } else {
        crate::reconcile::launch_executable()?
    };
    environment.insert("ST3_BIN".into(), st_binary.to_string_lossy().into_owned());
    if let Some(endpoint) = ENDPOINT.get() {
        environment.insert("ST3_ENDPOINT".into(), endpoint.clone());
    }
    Ok(environment)
}

/// Run variables for a check: each names this check where a run would name itself.
fn stand_in_variables(
    mission: &MissionSpec,
    workspace: &str,
    id: &str,
) -> BTreeMap<String, String> {
    let run = format!("check/{id}");
    let mut variables = crate::graph::runtime_proof_variables(mission);
    variables.extend([
        ("ST_MISSION_RUN".to_owned(), run.clone()),
        (
            "ST_ROOT_MISSION_RUN".to_owned(),
            format!("mission-run/{run}"),
        ),
        ("ST_ROOT_MISSION_RUN_ID".to_owned(), run),
        ("ST_RUN_GENERATION".to_owned(), format!("check-{id}")),
        ("ST_WORKSPACE".to_owned(), workspace.to_owned()),
        ("ST_REQUESTER".to_owned(), "person/requester".to_owned()),
        ("ST_ASSIGNEE".to_owned(), String::new()),
        ("ST_STEP".to_owned(), String::new()),
        ("ST_STEP_RUN".to_owned(), String::new()),
        ("PATH".to_owned(), "${PATH}".to_owned()),
    ]);
    variables.remove("ST_AGENT");
    variables.remove("ST_GATE");
    variables
}

/// What a check's gates expand with: run variables, the inputs nobody gave, and the workspace.
struct Scope<'a> {
    variables: &'a BTreeMap<String, String>,
    missing_inputs: &'a std::collections::BTreeSet<String>,
    workspace: &'a str,
}

/// Each exec gate of `mission`, its steps, nested missions and loops, in declaration order.
fn collect(mission: &MissionSpec, owner: String, scope: &Scope<'_>, items: &mut Vec<Item>) {
    let variables = scope.variables;
    for gate in &mission.gates {
        push(mission, &owner, gate, scope, items);
    }
    for id in &mission.display_order {
        let Some(step) = mission.steps.get(id) else {
            continue;
        };
        let mut step_variables = variables.clone();
        let generation = variables
            .get("ST_RUN_GENERATION")
            .cloned()
            .unwrap_or_default();
        step_variables.insert("ST_STEP".into(), step.path.clone());
        step_variables.insert(
            "ST_STEP_RUN".into(),
            format!("step-run/{generation}/{}", step.path),
        );
        let step_owner = format!("step {}", step.path);
        let step_scope = Scope {
            variables: &step_variables,
            ..*scope
        };
        for gate in &step.gates {
            push(mission, &step_owner, gate, &step_scope, items);
        }
        if let Some(nested) = step.nested_mission.as_deref() {
            collect(nested, step_owner.clone(), &step_scope, items);
        }
        if let Some(spec) = step.loop_spec.as_deref() {
            let loop_owner = format!("loop {}", spec.path);
            for gate in &spec.until {
                push(mission, &loop_owner, gate, &step_scope, items);
            }
            for round in [
                Some(&spec.round),
                spec.on_keep.as_ref(),
                spec.on_discard.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                collect(round, loop_owner.clone(), &step_scope, items);
            }
        }
    }
}

fn push(
    mission: &MissionSpec,
    owner: &str,
    gate: &GateSpec,
    scope: &Scope<'_>,
    items: &mut Vec<Item>,
) {
    let GateSpec::Mechanical { .. } = gate else {
        return;
    };
    let definition = serde_json::to_string(gate).unwrap_or_default();
    let missing = scope
        .missing_inputs
        .iter()
        .filter(|name| definition.contains(&format!("${{input.{name}}}")))
        .map(|name| format!("--input {name}=VALUE"))
        .collect::<Vec<_>>();
    let mut expanded = gate.clone();
    let expansion = crate::reconcile::expand_gate(&mut expanded, scope.variables, scope.workspace);
    let GateSpec::Mechanical {
        name,
        command,
        host,
        workspace: gate_workspace,
        environment,
        time_limit_ms,
    } = expanded
    else {
        return;
    };
    let mut view = GateCheckItemView {
        mission: mission.id.clone(),
        owner: owner.to_owned(),
        gate: name,
        host,
        workspace: gate_workspace,
        command,
        answer: "waiting".into(),
        ..GateCheckItemView::default()
    };
    if !missing.is_empty() {
        view.answer = "unchecked".into();
        view.reason = Some(format!(
            "it reads a mission input; give it with {}",
            missing.join(" ")
        ));
    } else if let Err(error) = expansion {
        view.answer = "unchecked".into();
        view.reason = Some(format!("its definition does not expand: {error:#}"));
    }
    items.push(Item {
        view,
        environment,
        time_limit_ms,
        log: PathBuf::new(),
        report: PathBuf::new(),
        child: None,
        started: None,
    });
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
