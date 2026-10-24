//! Native process and PTY operations shared by st2 and st3.
//!
//! This crate owns process identity and the `pty` session boundary: it starts a session with the
//! `pty` binary and reads and controls it through `pty-client`. It has no catalog, graph, claim, or
//! provider policy.
//!
//! Legacy revoked starts can be closed only through
//! [`PtyRuntime::legacy_start_closure`]: Linux captures complete manager/proc
//! evidence under the spawn lock. Keep the returned [`LegacyStartClosureProof`]
//! alive through the state writer; serialized evidence alone does not hold the
//! fence. Dry-run and apply each acquire a fresh proof and compare its stable
//! evidence. This API observes only; it never stops a process or unloads a scope.

mod environment;
mod isolate;
mod priority;
mod process;
mod procfs;
mod pty;
#[path = "../../pty-launch-environment.rs"]
mod pty_launch_environment;

pub use environment::{
    ShellStartupTimeout, expand_path_placeholder, is_shell_startup_timeout, login_environment,
    login_environment_from, login_environment_within, materialize_environment, overlay_environment,
    resolve_executable,
};
pub use isolate::{
    CgroupEnding, Isolation, SCOPE_GRACE, ScopeRetirementProof, end_scope, end_scope_with_proof,
    initialize_isolation, mode as isolation_mode, scope_unit, signal_scope, systemd_user_available,
    warn_if_degraded, wrap as wrap_isolated,
};
pub use priority::{
    BACKGROUND_IO_LEVEL, BACKGROUND_NICE, LIVE_WEIGHT, PRIORITY_ENV, SeatPriority, ServerPlacement,
    protect_server, protect_servers, report as priority_report, server_unit,
    set_configured as set_configured_seat_priority, work_prefix,
};
pub use process::{
    ExecGeneration, ExecObservation, ExecRuntime, StartRejected, process_start_token,
};
pub use procfs::ProcessNamespaceMismatch;
pub use pty::{
    Launch, LegacyStartClosureEvidence, LegacyStartClosureProof, LegacyStartProofUnavailable, ProcfsHidesProcesses, PtyObservation, PtyRuntime,
    PtySpawnTimeout, PtySpawnTimeoutPhase, TerminalSize,
};
