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
//! signals, and teardown.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
    match mode() {
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
}
