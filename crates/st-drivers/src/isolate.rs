//! Isolated task spawn (R21b) — the permanent fleet-fragility fix.
//!
//! st2 already survives its own *process* death (`setsid`, see [`crate::exec_backend`] and
//! `tests/nomad_survival.rs`). But `setsid` changes a process's **session**, not its **cgroup**.
//! systemd tears a unit down by cgroup, so restarting a supervisor unit would kill every task still
//! in that cgroup.
//!
//! The fix: spawn each task into its own OS supervision domain, independent of BOTH the spawner and
//! the transport daemon — one goal, per-OS mechanism.
//!
//! - **Linux with systemd 236+**: `systemd-run --user --scope --collect --quiet --unit=<unit>`
//!   `-- <task>`, with `--expand-environment=no` on v254+. Legacy scopes directly exec argv. The task runs in its own transient scope = its own cgroup,
//!   registered with the user manager as a **sibling** of the transport unit (a scope created inside
//!   a service lands at `app.slice/<unit>`, not nested under the service). A cascade kill of the
//!   transport unit's cgroup cannot reach a sibling. `--scope` (not `--service`) keeps st2 the logical
//!   supervisor — systemd provides only the cgroup; adoption/teardown/restart stay st2's. `--collect`
//!   GCs the scope once it empties.
//! - **macOS / unsupported Linux**: `setsid` + reparent to init/launchd is the fallback. Here [`wrap`]
//!   is a no-op pass-through; the caller's existing `setsid` (exec) or the `pty` daemon (pty) provides
//!   detachment. If isolation was wanted but the systemd user manager or the exact-argv capability
//!   is unavailable, we degrade to that pass-through and log a loud WARN — never a silent
//!   "isolated" claim.
//!
//! Teardown is unchanged: the scope is for **survival only**. `pty kill` / the exec process-group kill
//! still tear tasks down; the scope just prevents the transport from taking them as collateral.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// How a task is isolated from its spawner and the transport daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Linux with systemd 236+: own transient `--user` scope with opaque inner argv.
    Scope,
    /// macOS / non-systemd: `setsid` + reparent to init/launchd (no cgroup needed — the transport
    /// cannot cascade-kill a detached process on these platforms).
    Detached,
    /// Isolation was wanted (a Linux host) but the required systemd user scope capability is
    /// unavailable — degraded to `Detached` with a logged WARN. Distinct from `Detached` so
    /// callers/tests can tell an intended pass-through from a degraded one.
    DegradedDetached,
}

struct Configuration {
    mode: Isolation,
    expansion_flag: bool,
}

static CONFIGURATION: OnceLock<Configuration> = OnceLock::new();

/// The isolation mode for this host, detected once and cached.
pub fn mode() -> Isolation {
    CONFIGURATION.get_or_init(detect).mode
}

fn detect() -> Configuration {
    if !cfg!(target_os = "linux") {
        return Configuration {
            mode: Isolation::Detached,
            expansion_flag: false,
        };
    }
    if let Some(expansion_flag) = systemd_scope_available() {
        Configuration {
            mode: Isolation::Scope,
            expansion_flag,
        }
    } else {
        tracing::warn!(
            "st: systemd user scopes are unavailable — spawning tasks WITHOUT cgroup isolation. \
             A transport/supervisor restart may cascade-kill them. Enable a systemd user manager \
             (`loginctl enable-linger`) to restore isolation."
        );
        Configuration {
            mode: Isolation::DegradedDetached,
            expansion_flag: false,
        }
    }
}

fn systemd_scope_available() -> Option<bool> {
    std::env::var_os("XDG_RUNTIME_DIR")?;
    let output = Command::new("systemd-run")
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let expansion_flag = scope_expansion_flag(&output.stdout)?;
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success()
        .then_some(expansion_flag)
}

// v236 adds --collect. Legacy scopes pass argv directly to execvpe; do not
// escape percent or dollar bytes. v254+ supports explicitly disabling expansion.
fn scope_expansion_flag(output: &[u8]) -> Option<bool> {
    let mut words = std::str::from_utf8(output).ok()?.split_ascii_whitespace();
    if words.next()? != "systemd" {
        return None;
    }
    let version = words.next()?.parse::<u32>().ok()?;
    (version >= 236).then_some(version >= 254)
}

static SCOPE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, systemd-safe scope unit name for a task id. The name is **write-only** — st2 references it
/// only at spawn (`--unit=`); teardown is by process group / `pty kill` and adoption is by pidfile /
/// `pty list`, neither of which needs the scope name. So it must be UNIQUE, not deterministic: a
/// deterministic name collides whenever a scope of that name still lingers — a stale/failed scope on
/// re-spawn, or (in tests) a concurrent spawn of the same declared id — and `systemd-run --unit=<X>`
/// fails hard on an existing `<X>`. A per-process nonce (`<pid>-<seq>`) rules that out. The task id is
/// kept in the name purely for greppability in `systemctl --user list-units`. Non-`[A-Za-z0-9:_.-]`
/// bytes → `_`.
pub fn scope_unit(task_id: &str) -> String {
    let safe: String = task_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let seq = SCOPE_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("st-{safe}-{}-{seq}.scope", std::process::id())
}

/// Build the OUTER launch [`Command`] for the inner `program` + `args`, isolated under `unit`.
///
/// In [`Isolation::Scope`] this is `systemd-run --user --scope --collect --quiet --unit=<unit>
/// -- <program> <args>`, adding `--expand-environment=no` before `--` on v254+;
/// otherwise it is `<program> <args>` verbatim.
/// v254+ adds the expansion-disable option; older scopes already exec argv directly. This keeps
/// every inner argv element opaque, including dollar-bearing literals. Either way the caller applies env / cwd / stdio / `pre_exec` to the
/// returned Command and they reach the task — for `--scope`, scope mode runs the command in the
/// caller's context, so cwd, environment, and stdio fds all inherit (verified).
pub fn wrap(unit: &str, program: &OsStr, args: &[&OsStr]) -> Command {
    let configuration = CONFIGURATION.get_or_init(detect);
    wrap_for_mode(
        configuration.mode,
        configuration.expansion_flag,
        unit,
        program,
        args,
    )
}

fn wrap_for_mode(
    isolation: Isolation,
    expansion_flag: bool,
    unit: &str,
    program: &OsStr,
    args: &[&OsStr],
) -> Command {
    match isolation {
        Isolation::Scope => {
            let mut c = Command::new("systemd-run");
            c.args([
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--no-ask-password",
            ])
            .arg(format!("--unit={unit}"));
            if expansion_flag {
                c.arg("--expand-environment=no");
            }
            c.arg("--").arg(program).args(args);
            c
        }
        Isolation::Detached | Isolation::DegradedDetached => {
            let mut c = Command::new(program);
            c.args(args);
            c
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_scope_keeps_literal_percent_dollar_and_non_utf8_argv() {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes = std::ffi::OsString::from_vec(b"%n:$HOME:${UNSET}:$$:%%:\xff".to_vec());
        let arguments = [bytes.as_os_str(), OsStr::new(""), OsStr::new("two words")];
        let program = OsStr::new("/tmp/provider%$literal");
        let command = wrap_for_mode(
            Isolation::Scope,
            false,
            "st-studio.scope",
            program,
            &arguments,
        );
        let actual = command.get_args().collect::<Vec<_>>();
        let separator = actual
            .iter()
            .position(|arg| *arg == OsStr::new("--"))
            .unwrap();
        assert_eq!(actual[separator + 1], program);
        assert_eq!(&actual[separator + 2..], &arguments);
        assert!(!actual.contains(&OsStr::new("--expand-environment=no")));
        assert!(actual.contains(&OsStr::new("--no-ask-password")));
        assert_eq!(command.get_program(), OsStr::new("systemd-run"));
    }

    #[test]
    fn scope_unit_is_unique_and_systemd_safe() {
        // Keeps the (dotted) id for greppability, gains the st2- prefix, a nonce, and the .scope suffix.
        let u = scope_unit("example-linux.demo.agent");
        assert!(
            u.starts_with("st-example-linux.demo.agent-"),
            "unexpected unit name {u}"
        );
        assert!(u.ends_with(".scope"), "unexpected unit name {u}");
        // Unsafe bytes (space, slash) are replaced so systemd never rejects the unit name.
        assert!(scope_unit("a b/c").starts_with("st-a_b_c-"));
        // UNIQUE, not deterministic — two spawns of the same id never collide on the scope name.
        assert_ne!(scope_unit("x"), scope_unit("x"));
    }

    #[test]
    fn legacy_scopes_preserve_argv_and_newer_versions_support_the_expansion_flag() {
        for version in [236, 249, 252, 253] {
            assert_eq!(
                scope_expansion_flag(format!("systemd {version} (fixture)\n").as_bytes()),
                Some(false)
            );
        }
        for version in [254, 257, 260] {
            assert_eq!(
                scope_expansion_flag(format!("systemd {version} (fixture)\n").as_bytes()),
                Some(true)
            );
        }
        assert_eq!(scope_expansion_flag(b"systemd 235\n"), None);
        assert_eq!(scope_expansion_flag(b"unexpected output\n"), None);
    }

    #[test]
    fn wrap_scope_disables_expansion_and_preserves_dollar_bearing_argv() {
        let cmd = wrap_for_mode(
            Isolation::Scope,
            true,
            "st-x.scope",
            OsStr::new("provider"),
            &[
                OsStr::new("$HOME"),
                OsStr::new("${UNSET}"),
                OsStr::new("$$"),
            ],
        );
        let args: Vec<&OsStr> = cmd.get_args().collect();

        assert_eq!(cmd.get_program(), OsStr::new("systemd-run"));
        assert_eq!(
            args,
            vec![
                OsStr::new("--user"),
                OsStr::new("--scope"),
                OsStr::new("--collect"),
                OsStr::new("--quiet"),
                OsStr::new("--no-ask-password"),
                OsStr::new("--unit=st-x.scope"),
                OsStr::new("--expand-environment=no"),
                OsStr::new("--"),
                OsStr::new("provider"),
                OsStr::new("$HOME"),
                OsStr::new("${UNSET}"),
                OsStr::new("$$"),
            ]
        );
    }

    #[test]
    fn wrap_detached_modes_preserve_exact_program_and_argv() {
        for isolation in [Isolation::Detached, Isolation::DegradedDetached] {
            let cmd = wrap_for_mode(
                isolation,
                false,
                "unused.scope",
                OsStr::new("provider"),
                &[
                    OsStr::new("$HOME"),
                    OsStr::new("${UNSET}"),
                    OsStr::new("$$"),
                ],
            );
            let args: Vec<&OsStr> = cmd.get_args().collect();

            assert_eq!(cmd.get_program(), OsStr::new("provider"));
            assert_eq!(
                args,
                vec![
                    OsStr::new("$HOME"),
                    OsStr::new("${UNSET}"),
                    OsStr::new("$$"),
                ]
            );
        }
    }
}
