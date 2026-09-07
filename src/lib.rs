//! st2 — a harness-agnostic runner over a unified catalog+inbox folder.
//!
//! st2 reads a catalog of hand-authored agent declarations plus each agent's inbox. It keeps every
//! declared task running and delivers native messages. Harness-specific behavior stays explicit in
//! each declaration's command, environment, hooks, and workspace materialization block.

pub mod agent_author;
pub mod agent_publish;
pub mod agents;
pub mod catalog;
pub mod catalog_archive;
pub mod catalog_graph;
pub mod catalog_lock;
pub mod catalog_transaction;
pub mod claude_channel;
pub mod claude_mcp;
pub mod claude_session;
pub mod codex_app_server;
pub mod context;
pub mod delivery_ledger;
pub mod ding;
pub mod driver;
pub mod driver_diagnostic;
pub mod eval_run;
pub mod eval_spec;
pub mod event;
pub mod exec_backend;
pub mod expand;
pub mod flapping;
pub mod harness_context;
pub mod harness_state;
pub mod harness_version;
pub mod hooks;
pub mod host_lock;
pub mod identity;
pub mod isolate;
pub mod materialize;
pub mod message;
pub mod omp_session;
pub mod metrics;
pub mod migrations;
pub mod opencode_session;
pub mod park;
pub mod pi_channel;
pub mod pi_session;
pub mod pretrust;
pub mod provider_session;
pub mod reconcile;
pub mod request;
pub mod resource_observe;
pub mod resource_profile;
pub mod resource_profile_supervisor;
pub mod resync;
pub mod run;
pub mod service;
pub mod status;
pub mod supervisor_chain;
pub mod task_inventory;
pub mod telemetry;
pub mod validate;
pub mod version;
mod watch;

// The declaration model and the catalog walk live in the `agent-spec` crate, so st2 and any other
// reader of the same catalog share one implementation. Re-exported under their original paths:
// `st2::spec::…` / `st2::discovery::…` keep working for the binary and the test suite.
pub use agent_spec::{discovery, spec};

pub use agent_spec::discovery::{Discovered, SpecError, discover, discover_file, discover_strict};
pub use agent_spec::spec::{
    AgentDesiredState, AgentSpec, ClaudeDriver, CodexDriver, DeliveryTransport, Driver, JobType,
    OmpDriver, OpenCodeDriver, PiDriver, Resource, Restart, RestartMode, SessionDriver, Task,
    TaskKind, TaskLifecycle, parse_duration,
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
    up_loop, up_loop_specs, up_once, up_once_selected, up_once_selected_specs, up_once_specs,
};
