//! Shared task isolation for st2 and st3.
//!
//! A new process session survives a parent process exit. It does not escape a Linux service cgroup.
//!
//! Linux uses a transient systemd user scope when the user manager is available. The scope is a
//! sibling of the daemon service, so a daemon cgroup restart does not stop the task.
//!
//! Other Unix hosts use a detached process session. Linux uses the same fallback when its user
//! manager is unavailable, but reports that mode as degraded.
//!
//! The scope provides survival and descendant containment. The runtimes still own adoption,
//! signals, and teardown. Teardown ends with the scope: [`end_scope`] ends every process a task
//! started, including one that left its process tree or started a process group of its own.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    Scope,
    Detached,
    DegradedDetached,
}

static MODE: OnceLock<Isolation> = OnceLock::new();
static WARNED: AtomicBool = AtomicBool::new(false);
static SCOPE_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn mode() -> Isolation {
    *MODE.get_or_init(detect)
}

/// Select daemon isolation before constructing runtimes, using its captured login environment.
/// Other callers retain the existing ambient-environment default.
pub fn initialize_isolation(environment: &std::collections::BTreeMap<String, String>) -> Isolation {
    *MODE.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            Isolation::Detached
        } else if systemd_user_available_in(environment) {
            Isolation::Scope
        } else {
            Isolation::DegradedDetached
        }
    })
}

fn systemd_user_available_in(environment: &std::collections::BTreeMap<String, String>) -> bool {
    if !environment.contains_key("XDG_RUNTIME_DIR") {
        return false;
    }
    [
        ("systemd-run", vec!["--version"]),
        ("systemctl", vec!["--user", "show-environment"]),
    ]
    .into_iter()
    .all(|(program, arguments)| {
        let Ok(program) = crate::resolve_executable(program, environment) else {
            return false;
        };
        Command::new(program)
            .args(arguments)
            .env_clear()
            .envs(environment)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

pub fn warn_if_degraded(product: &str) {
    if mode() == Isolation::DegradedDetached && !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "{product}: WARN systemd user scopes are unavailable; tasks lack cgroup isolation. A daemon restart can stop them. Enable a user systemd manager with linger."
        );
    }
}

fn detect() -> Isolation {
    if cfg!(target_os = "linux") {
        if systemd_user_available() {
            Isolation::Scope
        } else {
            Isolation::DegradedDetached
        }
    } else {
        Isolation::Detached
    }
}

pub fn systemd_user_available() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    if !Command::new("systemd-run")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return false;
    }
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn scope_unit(product: &str, task_id: &str) -> String {
    let safe_product = sanitize(product);
    let safe_task = sanitize(task_id);
    let sequence = SCOPE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "{safe_product}-{safe_task}-{}-{sequence}.scope",
        std::process::id()
    )
}

pub(crate) fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ':' | '_' | '.' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub fn wrap(unit: &str, program: &OsStr, arguments: &[&OsStr]) -> Command {
    wrap_for_mode(mode(), unit, program, arguments)
}

fn wrap_for_mode(isolation: Isolation, unit: &str, program: &OsStr, arguments: &[&OsStr]) -> Command {
    match isolation {
        Isolation::Scope => {
            let mut command = Command::new("systemd-run");
            command
                .args([
                    "--user",
                    "--scope",
                    "--collect",
                    "--quiet",
                    "--expand-environment=no",
                ])
                .arg(format!("--unit={unit}"))
                .arg("--description=st seat")
                .arg("--")
                .arg(program)
                .args(arguments);
            command
        }
        Isolation::Detached | Isolation::DegradedDetached => {
            let mut command = Command::new(program);
            command.args(arguments);
            command
        }
    }
}

/// How long the processes left in a work scope get to exit on SIGTERM before SIGKILL.
pub const SCOPE_GRACE: Duration = Duration::from_secs(2);
/// How long a scope gets to empty after SIGKILL.
const SCOPE_KILL_WAIT: Duration = Duration::from_secs(2);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Work scopes this process has seen empty. A scope holds only what started in it, so an empty
/// one stays empty, and systemd collects it.
static ENDED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Whether `unit` is a work scope [`scope_unit`] named: never a PTY server's own scope, a
/// service, or the unit this process runs in.
fn is_work_scope(unit: &str) -> bool {
    unit.starts_with("st3-")
        && unit.ends_with(".scope")
        && !unit.starts_with("st3-pty-server-")
        && !unit.contains('/')
        && own_unit().as_deref() != Some(unit)
}

fn own_unit() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(path.rsplit('/').next()?.to_owned())
}

/// Whether this process has seen the work scope `unit` empty.
pub(crate) fn has_ended(unit: &str) -> bool {
    ENDED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(unit)
}

fn mark_ended(unit: &str) {
    let mut ended = ENDED.lock().unwrap_or_else(PoisonError::into_inner);
    // A forgotten scope costs one `systemctl show` to learn again.
    if ended.len() > 4096 {
        ended.clear();
    }
    ended.insert(unit.to_owned());
}

/// Ends every process in the work scope `unit`: SIGTERM, up to `grace` for them to exit, then
/// SIGKILL. A scope that is not loaded has ended, since `--collect` removes an empty one.
///
/// Returns whether the scope is now empty. It is false, and nothing is signalled, on a host
/// without systemd user scopes or for a unit that is not a work scope.
pub fn end_scope(unit: &str, grace: Duration) -> Result<bool> {
    if mode() != Isolation::Scope || !is_work_scope(unit) {
        return Ok(false);
    }
    if has_ended(unit) {
        return Ok(true);
    }
    let Some(cgroup) = control_group(unit)? else {
        mark_ended(unit);
        return Ok(true);
    };
    if !grace.is_zero() && populated(&cgroup) {
        kill_unit(unit, libc::SIGTERM)?;
        wait_until_empty(&cgroup, grace);
    }
    if populated(&cgroup) {
        kill_unit(unit, libc::SIGKILL)?;
        anyhow::ensure!(
            wait_until_empty(&cgroup, SCOPE_KILL_WAIT),
            "processes in {unit} survived SIGKILL for {}s",
            SCOPE_KILL_WAIT.as_secs()
        );
    }
    mark_ended(unit);
    Ok(true)
}

type Job = Box<dyn FnOnce() -> Result<()> + Send>;

/// The jobs [`end_later`] queued, their keys, and whether a worker drains them.
struct Later {
    queue: VecDeque<(String, Job)>,
    keys: BTreeSet<String>,
    working: bool,
}

static LATER: Mutex<Later> = Mutex::new(Later {
    queue: VecDeque::new(),
    keys: BTreeSet::new(),
    working: false,
});

/// Runs `job`, which ends a scope, on one background worker, so a caller that may not wait, such
/// as a reconcile pass, never waits out a grace. A job already queued under `key` is not queued
/// twice.
pub(crate) fn end_later(key: String, job: impl FnOnce() -> Result<()> + Send + 'static) {
    let mut later = LATER.lock().unwrap_or_else(PoisonError::into_inner);
    if !later.keys.insert(key.clone()) {
        return;
    }
    later.queue.push_back((key, Box::new(job)));
    if std::mem::replace(&mut later.working, true) {
        return;
    }
    drop(later);
    std::thread::spawn(|| {
        loop {
            let next = {
                let mut later = LATER.lock().unwrap_or_else(PoisonError::into_inner);
                let next = later.queue.pop_front();
                later.working = next.is_some();
                next
            };
            let Some((key, job)) = next else {
                return;
            };
            if let Err(error) = job() {
                eprintln!("st3: WARN {error:#}");
            }
            LATER
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .keys
                .remove(&key);
        }
    });
}

/// Sends `signal` to every process in the work scope `unit`. Returns whether it was sent: it is
/// not where [`end_scope`] would do nothing, or once the scope has ended.
pub fn signal_scope(unit: &str, signal: i32) -> Result<bool> {
    if mode() != Isolation::Scope || !is_work_scope(unit) || has_ended(unit) {
        return Ok(false);
    }
    kill_unit(unit, signal)
}

/// The cgroup directory of `unit`, or none once the unit is not loaded.
fn control_group(unit: &str) -> Result<Option<PathBuf>> {
    let mut command = Command::new("systemctl");
    command.args([
        "--user",
        "show",
        "--property=LoadState",
        "--property=ControlGroup",
        unit,
    ]);
    let output = crate::pty::output_within(command, SYSTEMCTL_TIMEOUT)
        .with_context(|| format!("ask the systemd user manager about {unit}"))?;
    anyhow::ensure!(
        output.status.success(),
        "the systemd user manager could not show {unit}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(parse_control_group(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_control_group(show: &str) -> Option<PathBuf> {
    let value = |key: &str| {
        show.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
    };
    let cgroup = value("ControlGroup");
    (value("LoadState") == "loaded" && cgroup.starts_with('/'))
        .then(|| Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/')))
}

/// Whether the cgroup still holds a process. A cgroup that is gone holds none; one that cannot
/// be read counts as populated, so the caller never reports an end it did not see.
fn populated(cgroup: &Path) -> bool {
    match std::fs::read_to_string(cgroup.join("cgroup.events")) {
        Ok(events) => events.lines().any(|line| line.trim() == "populated 1"),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

fn wait_until_empty(cgroup: &Path, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !populated(cgroup) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `systemctl kill` signals every process in the unit's cgroup. Returns false when the unit was
/// no longer loaded.
fn kill_unit(unit: &str, signal: i32) -> Result<bool> {
    let name = match signal {
        libc::SIGTERM => "SIGTERM".to_owned(),
        libc::SIGKILL => "SIGKILL".to_owned(),
        other => other.to_string(),
    };
    let mut command = Command::new("systemctl");
    command
        .args(["--user", "kill"])
        .arg(format!("--signal={name}"))
        .arg(unit);
    let output = crate::pty::output_within(command, SYSTEMCTL_TIMEOUT)
        .with_context(|| format!("ask the systemd user manager to signal {unit}"))?;
    if output.status.success() {
        return Ok(true);
    }
    let error = String::from_utf8_lossy(&output.stderr);
    if error.contains("not loaded") {
        mark_ended(unit);
        return Ok(false);
    }
    anyhow::bail!(
        "the systemd user manager could not send {name} to {unit}: {}",
        error.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_probe_resolves_tools_from_the_captured_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let environment = std::collections::BTreeMap::from([
            ("PATH".into(), root.path().display().to_string()),
            ("XDG_RUNTIME_DIR".into(), root.path().display().to_string()),
            ("ORCHID_CONTROL".into(), "captured".into()),
        ]);
        assert!(!systemd_user_available_in(&environment));
        for program in ["systemd-run", "systemctl"] {
            let path = root.path().join(program);
            std::fs::write(&path, "#!/bin/sh\n[ \"$ORCHID_CONTROL\" = captured ]\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(systemd_user_available_in(&environment));
    }

    #[test]
    fn scope_names_are_unique_and_safe() {
        let first = scope_unit("st3", "node/demo task");
        let second = scope_unit("st3", "node/demo task");
        assert!(first.starts_with("st3-node_demo_task-"));
        assert!(first.ends_with(".scope"));
        assert_ne!(first, second);
    }

    #[test]
    fn only_a_work_scope_st_named_is_ended() {
        assert!(is_work_scope(&scope_unit("st3", "fleet/demo worker")));
        assert!(!is_work_scope("st3-pty-server-fleet_demo_worker-42.scope"));
        assert!(!is_work_scope("st3.service"));
        assert!(!is_work_scope("app.slice"));
        assert!(!is_work_scope("st3-../app.slice/st3.scope"));
        if let Some(own) = own_unit() {
            assert!(!is_work_scope(&own));
        }
    }

    #[test]
    fn a_unit_that_is_not_loaded_has_no_cgroup() {
        assert_eq!(
            parse_control_group(
                "LoadState=loaded\nControlGroup=/user.slice/user-1000.slice/user@1000.service/app.slice/st3-w-1-0.scope\n"
            ),
            Some(PathBuf::from(
                "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/st3-w-1-0.scope"
            ))
        );
        assert_eq!(
            parse_control_group("LoadState=not-found\nControlGroup=\n"),
            None
        );
        assert_eq!(
            parse_control_group("LoadState=loaded\nControlGroup=\n"),
            None
        );
    }

    #[test]
    fn wrapper_matches_the_detected_mode() {
        let command = wrap(
            "st3-work.scope",
            OsStr::new("sh"),
            &[OsStr::new("-c"), OsStr::new("true")],
        );
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        match mode() {
            Isolation::Scope => {
                assert_eq!(command.get_program(), OsStr::new("systemd-run"));
                assert!(arguments.contains(&"--scope".to_owned()));
                assert!(arguments.contains(&"--unit=st3-work.scope".to_owned()));
                assert!(arguments.contains(&"--expand-environment=no".to_owned()));
            }
            Isolation::Detached | Isolation::DegradedDetached => {
                assert_eq!(command.get_program(), OsStr::new("sh"));
                assert_eq!(arguments, ["-c", "true"]);
            }
        }
    }

    #[test]
    fn scope_keeps_environment_values_out_of_argv_and_description() {
        let mut command = wrap_for_mode(
            Isolation::Scope,
            "st3-work.scope",
            OsStr::new("sh"),
            &[OsStr::new("-c"), OsStr::new("test \"$SEAT_MARKER\" = \"$EXPECTED\"")],
        );
        command.env("SEAT_MARKER", "synthetic-marker");
        command.env("EXPECTED", "synthetic-marker");
        let arguments = command.get_args().collect::<Vec<_>>();
        let separator = arguments.iter().position(|argument| *argument == "--").unwrap();
        assert!(arguments.iter().all(|argument| {
            let argument = argument.to_string_lossy();
            !argument.contains("synthetic-marker") && !argument.contains("--env")
                && !argument.contains("EnvironmentFile")
        }));
        assert!(arguments[..separator].contains(&OsStr::new("--description=st seat")));
    }
}
