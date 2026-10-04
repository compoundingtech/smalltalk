//! Native process and PTY operations shared by st2 and st3.
//!
//! This crate owns process identity and the `pty` session boundary: it starts a session with the
//! `pty` binary and reads and controls it through `pty-client`. It has no catalog, graph, claim, or
//! provider policy.

mod environment;
mod isolate;
mod priority;
mod process;
mod pty;

pub use environment::{
    ShellStartupTimeout, expand_path_placeholder, is_shell_startup_timeout, login_environment, login_environment_from,
    login_environment_within, materialize_environment, overlay_environment, resolve_executable,
};
pub use isolate::{
    Isolation, SCOPE_GRACE, end_scope, initialize_isolation, mode as isolation_mode, scope_unit,
    signal_scope, systemd_user_available, warn_if_degraded, wrap as wrap_isolated,
};
pub use priority::{
    LIVE_WEIGHT, ServerPlacement, protect_server, protect_servers, report as priority_report,
    server_unit, work_prefix,
};
pub use process::{ExecGeneration, ExecObservation, ExecRuntime, process_start_token};
pub use pty::{Launch, PtyObservation, PtyRuntime, PtySpawnTimeout, PtySpawnTimeoutPhase};
