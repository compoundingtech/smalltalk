//! Native process and PTY operations shared by st2 and st3.
//!
//! This crate owns process identity and the `pty` session boundary: it starts a session with the
//! `pty` binary and reads and controls it through `pty-client`. It has no catalog, graph, claim, or
//! provider policy.

mod environment;
mod isolate;
mod process;
mod pty;

pub use environment::{
    expand_path_placeholder, login_environment, materialize_environment, resolve_executable,
};
pub use isolate::{
    Isolation, mode as isolation_mode, scope_unit, systemd_user_available, warn_if_degraded,
    wrap as wrap_isolated,
};
pub use process::{ExecGeneration, ExecObservation, ExecRuntime, process_start_token};
pub use pty::{Launch, PtyObservation, PtyRuntime, PtySpawnTimeout, PtySpawnTimeoutPhase};
