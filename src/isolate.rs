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
//! - **Linux with systemd 254+**: `systemd-run --user --scope --collect --quiet --unit=<unit>`
//!   `--expand-environment=no -- <task>`. The task runs in its own transient scope = its own cgroup,
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
    /// Linux with systemd 254+: own transient `--user` scope with opaque inner argv.
    Scope,
    /// macOS / non-systemd: `setsid` + reparent to init/launchd (no cgroup needed — the transport
    /// cannot cascade-kill a detached process on these platforms).
    Detached,
    /// Isolation was wanted (a Linux host) but the required systemd user scope capability is
    /// unavailable — degraded to `Detached` with a logged WARN. Distinct from `Detached` so
    /// callers/tests can tell an intended pass-through from a degraded one.
    DegradedDetached,
}

static MODE: OnceLock<Isolation> = OnceLock::new();

/// The isolation mode for this host, detected once and cached.
pub fn mode() -> Isolation {
    *MODE.get_or_init(detect)
}

fn detect() -> Isolation {
    if cfg!(target_os = "linux") {
        if systemd_user_available() {
            Isolation::Scope
        } else {
            // The old line embedded a manual "WARN" prefix; the facade carries severity now.
            tracing::warn!(
                "st2: systemd user scopes with opaque argv unavailable (`systemd-run` 254+ and \
                 $XDG_RUNTIME_DIR are required) — spawning tasks WITHOUT cgroup isolation. A \
                 transport/supervisor restart may cascade-kill them. Upgrade systemd and enable a \
                 user manager (`loginctl enable-linger`) to restore isolation."
            );
            Isolation::DegradedDetached
        }
    } else {
        // macOS et al: setsid + reparent is the whole defense; no cgroups to escape.
        Isolation::Detached
    }
}

/// Whether a `--user` systemd scope can preserve the inner argv exactly.
fn systemd_user_available() -> bool {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    Command::new("systemd-run")
        .args(["--user", "--version"])
        .stderr(Stdio::null())
        .output()
        .map(|output| {
            output.status.success() && systemd_version_supports_exact_argv(&output.stdout)
        })
        .unwrap_or(false)
}

fn systemd_version_supports_exact_argv(output: &[u8]) -> bool {
    std::str::from_utf8(output)
        .ok()
        .and_then(|output| output.split_ascii_whitespace().nth(1))
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 254)
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
    format!("st2-{safe}-{}-{seq}.scope", std::process::id())
}

/// Build the OUTER launch [`Command`] for the inner `program` + `args`, isolated under `unit`.
///
/// In [`Isolation::Scope`] this is `systemd-run --user --scope --collect --quiet --unit=<unit>
/// --expand-environment=no -- <program> <args>`; otherwise it is `<program> <args>` verbatim.
/// Disabling systemd's environment expansion keeps every inner argv element opaque, including
/// dollar-bearing literals. Either way the caller applies env / cwd / stdio / `pre_exec` to the
/// returned Command and they reach the task — for `--scope`, scope mode runs the command in the
/// caller's context, so cwd, environment, and stdio fds all inherit (verified).
pub fn wrap(unit: &str, program: &OsStr, args: &[&OsStr]) -> Command {
    wrap_for_mode(mode(), unit, program, args)
}

fn wrap_for_mode(isolation: Isolation, unit: &str, program: &OsStr, args: &[&OsStr]) -> Command {
    match isolation {
        Isolation::Scope => {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--collect", "--quiet"])
                .arg(format!("--unit={unit}"))
                .arg("--expand-environment=no")
                .arg("--")
                .arg(program)
                .args(args);
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
    fn scope_unit_is_unique_and_systemd_safe() {
        // Keeps the (dotted) id for greppability, gains the st2- prefix, a nonce, and the .scope suffix.
        let u = scope_unit("hetz.demo.agent");
        assert!(
            u.starts_with("st2-hetz.demo.agent-"),
            "unexpected unit name {u}"
        );
        assert!(u.ends_with(".scope"), "unexpected unit name {u}");
        // Unsafe bytes (space, slash) are replaced so systemd never rejects the unit name.
        assert!(scope_unit("a b/c").starts_with("st2-a_b_c-"));
        // UNIQUE, not deterministic — two spawns of the same id never collide on the scope name.
        assert_ne!(scope_unit("x"), scope_unit("x"));
    }

    #[test]
    fn exact_argv_requires_systemd_254_or_newer() {
        assert!(!systemd_version_supports_exact_argv(
            b"systemd 249 (249.11)\n"
        ));
        assert!(!systemd_version_supports_exact_argv(
            b"systemd 252 (252.38)\n"
        ));
        assert!(systemd_version_supports_exact_argv(
            b"systemd 254 (254.5)\n"
        ));
        assert!(systemd_version_supports_exact_argv(
            b"systemd 257 (257.7)\n"
        ));
        assert!(!systemd_version_supports_exact_argv(b"unexpected output\n"));
    }

    #[test]
    fn wrap_scope_disables_expansion_and_preserves_dollar_bearing_argv() {
        let cmd = wrap_for_mode(
            Isolation::Scope,
            "st2-x.scope",
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
                OsStr::new("--unit=st2-x.scope"),
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
