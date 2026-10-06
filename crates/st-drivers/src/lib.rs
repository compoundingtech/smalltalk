//! Harness drivers, native delivery, and catalog support shared by st2 and st3.
//!
//! This is the single home for new harness-driver work. The root st2 crate re-exports
//! its public APIs for compatibility; st3 imports this crate directly.
//!
//! Catalog reconciliation and resource-profile supervision remain coupled to the
//! shared mailbox and validation paths. They move intact here; separating support
//! that st3 does not call directly is later cleanup tracked by smalltalk issue #819.
//! Helpers exposed only for the remaining root modules are hidden from public docs.
//!
//! st2 reads a catalog of hand-authored agent declarations plus each agent's inbox. It keeps every
//! declared task running and delivers native messages. Harness-specific behavior stays explicit in
//! each declaration's command, environment, hooks, and workspace materialization block.

pub mod account;
pub mod capture_admission;
pub mod catalog;
pub mod catalog_archive;
pub mod catalog_lock;
pub mod catalog_transaction;
pub mod claude_channel;
pub mod claude_mcp;
pub mod claude_session;
pub mod codex_app_server;
pub mod context;
pub mod contracts;
pub mod delivery_ledger;
pub mod ding;
pub mod direct_actor;
pub mod driver;
pub mod driver_diagnostic;
pub mod driver_paths;
pub mod eval_spec;
pub mod event;
pub mod exec_backend;
pub mod expand;
pub mod flapping;
/// Private on purpose: every consumer is a sibling module in this crate, and the one site that
/// deliberately keeps its own `flock` is documented in the module itself.
mod flock;
/// Atomic filesystem publication shared with the root st2 request API.
#[doc(hidden)]
pub mod fsatomic;
pub mod harness_admission;
pub mod harness_context;
pub mod harness_events;
pub mod blocking_screen;
pub mod harness_state;
pub mod harness_timeline;
pub mod harness_version;
pub mod hooks;
pub mod host_lock;
pub mod identity;
pub mod isolate;
pub mod materialize;
pub mod message;
pub mod metrics;
pub mod migrations;
/// The stdio framing shared by the native channels; crate-internal.
mod native_channel;
pub mod omp_session;
pub mod opencode_session;
pub mod park;
pub mod pi_channel;
/// The launch body shared by the pi-family wrappers; crate-internal, reached through
/// `pi_session::run` and `omp_session::run`.
mod pi_family_session;
pub mod pi_session;
pub mod pretrust;
pub mod provider_session;
pub mod push_mailbox;
pub mod reconcile;
pub mod reexec;

pub mod residency;
pub mod residency_host;
pub mod resource_observe;
pub mod resource_profile;
pub mod resource_profile_supervisor;
pub mod resync;
pub mod run;

pub mod session_control;
pub mod status;
pub mod subagents;
pub mod supervisor_chain;
pub mod task_inventory;
pub mod telemetry;
pub mod validate;
pub mod version;
mod watch;

// The declaration model and the catalog walk live in the `agent-spec` crate, so st2 and any other
// reader of the same catalog share one implementation. Re-exported under their original paths:
// `st2::spec::…` / `st2::discovery::…` keep working for the binary and the test suite.
pub use agent_spec::kdl_version;
pub use agent_spec::{discovery, spec};

pub use agent_spec::discovery::{Discovered, SpecError, discover, discover_file, discover_strict};
pub use agent_spec::spec::{
    AgentDesiredState, AgentSpec, ClaudeDriver, CodexDriver, DeliveryTransport, Driver, JobType,
    OmpDriver, OpenCodeDriver, PiDriver, ResidencyPolicy, Resource, Restart, RestartMode,
    SessionDriver, Task, TaskKind, TaskLifecycle, parse_duration,
};
pub use catalog_lock::CatalogLock;
pub use exec_backend::ExecBackend;
pub use expand::{expand_env, expand_vars};
pub use flapping::FlappingCap;
pub use host_lock::HostLock;
pub use reconcile::{
    Launch, PtyPresentation, ReconcilePlan, Session, TaskLaunch, TaskTarget, Teardown, reconcile,
};
pub use run::{
    PtyCli, Runner, SystemRunner, UpReport, detect_host, down, down_specs, exec_state_dir, execute,
    up_loop, up_loop_specs, up_loop_with_residency, up_once, up_once_selected,
    up_once_selected_specs, up_once_specs, up_once_with_residency,
};
