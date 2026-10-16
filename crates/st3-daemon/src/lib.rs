//! st3 claims graph, API, reconciliation, and CLI support.

pub use st3_kernel::cli_output;
// cli_output exports these macros at its crate root; this crate exported them at its own.
pub use st3_kernel::{cli_print, cli_println};

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

pub use st3_kernel::accounts;
pub mod api;
pub use st3_kernel::archive;
pub mod attach_image;
pub mod backup;
pub mod blobs;
pub use st3_kernel::boot;
/// Git-backed change metadata and exact-OID atomic state transitions.
pub mod changes;
pub(crate) mod checkout;
pub mod claude_channel;
pub mod client;
pub mod config;
pub use st3_kernel::conversation_search;
pub mod creation;
pub mod repositories;
pub(crate) use st3_kernel::disk;
/// Answers the hooks an st3 seat's harness runs: `st driver-hook NAME`.
pub mod delivery_hold;
pub mod driver_hook;
pub mod environment;
pub use st3_kernel::evidence;
pub(crate) mod external_sessions;
pub(crate) use st3_kernel::native_views;
pub(crate) use st3_kernel::conversation_header;
pub(crate) mod subagent_sessions;
pub use st3_kernel::fleet;
/// Silent messages are stored and readable without waking the recipient.
pub mod silent;
/// `st missions check`: run a mission file's exec gates once, now, the way a run would.
pub mod gate_check;
pub use st3_kernel::gate_kinds;
pub use st3_kernel::gate_report;
pub mod github_watch;
pub mod graph;
pub mod handoff;
pub(crate) mod graph_watch_ivm;
pub use st3_kernel::harness_events;
pub use st3_kernel::hooks;
pub mod incremental;
pub use st3_kernel::instance;
pub use st3_kernel::lane;
pub mod mailbox;
pub mod message_body;
/// Bounded local maintenance workers shared by daemon startup and lifecycle fixtures.
pub mod maintenance;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub mod malloc_trim;
pub(crate) use st3_kernel::memory;
pub use st3_kernel::read_cache;
pub mod mission;
pub use st3_kernel::model;
/// A driver relaunches its harness on the native session a suspended seat resumes.
pub mod native_resume;
/// Durable seat-scoped first native launch outcomes.
pub mod native_seed;
pub mod node_identity;
pub mod objectives;
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
pub mod omp_ask_resume;
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
pub mod otel;
pub mod otel_client_class;
pub mod otlp;
pub mod peer;
pub mod placement;
pub mod person_request;
pub use st3_kernel::pricing;
pub use smallclaims::{performance, profile};
pub use st3_kernel::projection;
pub mod provenance;
pub mod reconcile;
mod reconcile_telemetry;
mod relay_trace;
pub use st3_kernel::rules;
/// Observes git and gh calls without changing their command behavior.
pub mod recorder;
/// Imports local command receipts into durable resource observations.
pub mod recorder_receipts;
/// Summarizes command recorder logs from one or more hosts.
pub mod recorder_report;
/// Finds references a publication or the graph names that do not resolve.
pub(crate) mod references;
/// Attaches a local terminal to a terminal another fleet host owns.
pub mod remote_terminal;
pub mod render;
pub mod resource;
pub mod rollout;
pub use st3_kernel::seat_queue;
/// Runs any CLI with credentials no seat can read, through a gateway the sekrets user owns.
pub mod sekrets;
pub mod service;
pub use st3_kernel::skill;
pub mod slo;
pub mod slo_history;
pub mod startup;
pub mod startup_telemetry;
pub mod store;
pub mod subagents;
/// Suspends a quiet seat and resumes its own native session.
pub mod suspension;
pub(crate) use st3_kernel::publication;
pub mod seat_snapshot;
pub mod telemetry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod kernel_tests;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;
pub use st3_kernel::terminal_binding;
/// Runs each agent as its own Unix user, in its own namespaces and root-owned cgroup.
pub mod tiles;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};

/// Shared authentication and transport for daemon GitHub callers.
mod github_http;
use st3_kernel::github_admission;
