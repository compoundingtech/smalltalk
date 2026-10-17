//! st3 claims graph, API, reconciliation, and CLI support.

pub use st3_kernel::cli_output;
// cli_output exports these macros at its crate root; this crate exported them at its own.
pub use st3_kernel::{cli_print, cli_println};

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
#[allow(dead_code)]
mod ci_test_paths;

pub use st3_kernel::accounts;
pub mod api;
pub use st3_kernel::archive;
pub(crate) use st3_store::client_values;
pub mod attach_image;
pub use st3_store::backup;
pub use st3_store::blobs;
pub use st3_kernel::boot;
/// Git-backed change metadata and exact-OID atomic state transitions.
pub use st3_store::changes;
pub(crate) use st3_store::checkout;
pub mod claude_channel;
pub use st3_store::client;
pub use st3_kernel::config;
pub use st3_kernel::conversation_search;
pub mod creation;
pub mod repositories;
pub(crate) use st3_kernel::disk;
/// Answers the hooks an st3 seat's harness runs: `st driver-hook NAME`.
pub mod delivery_hold;
pub(crate) mod delivery_presence;
pub mod driver_hook;
pub use st3_store::environment;
pub use st3_kernel::evidence;
pub(crate) use st3_store::external_sessions;
pub(crate) use st3_kernel::conversation_header;
pub(crate) mod subagent_sessions;
pub use st3_kernel::fleet;
/// Silent messages are stored and readable without waking the recipient.
pub use st3_store::silent;
/// `st missions check`: run a mission file's exec gates once, now, the way a run would.
pub mod gate_check;
pub(crate) use st3_store::gate_parse;
pub use st3_kernel::gate_kinds;
pub use st3_kernel::gate_report;
pub use st3_store::github_watch;
pub use st3_store::graph;
pub mod handoff;
pub(crate) mod graph_watch_ivm;
pub use st3_kernel::harness_events;
pub use st3_kernel::hooks;
pub use st3_telemetry::incremental;
pub use st3_kernel::instance;
pub use st3_kernel::lane;
pub use st3_store::mailbox;
pub use st3_store::message_body;
/// Bounded local maintenance workers shared by daemon startup and lifecycle fixtures.
pub mod maintenance;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub mod malloc_trim;
pub(crate) use st3_kernel::memory;
pub use st3_kernel::read_cache;
pub use st3_store::mission;
pub use st3_kernel::model;
/// A driver relaunches its harness on the native session a suspended seat resumes.
pub use st3_store::native_resume;
/// Durable seat-scoped first native launch outcomes.
pub mod native_seed;
pub mod node_identity;
pub mod objectives;
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
pub mod omp_ask_resume;
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
pub use st3_telemetry::otel;
pub use st3_telemetry::otel_client_class;
pub use st3_telemetry::otlp;
pub mod peer;
pub mod placement;
pub use st3_store::person_request;
pub use st3_kernel::pricing;
pub use smallclaims::{performance, profile};
pub use st3_kernel::projection;
pub use st3_store::provenance;
pub mod reconcile;
pub(crate) use st3_telemetry::reconcile_telemetry;
pub(crate) use st3_telemetry::relay_trace;
pub use st3_kernel::rules;
pub use st3_telemetry::recorder;
/// Imports local command receipts into durable resource observations.
pub use st3_store::recorder_receipts;
pub use st3_telemetry::recorder_report;
/// Finds references a publication or the graph names that do not resolve.
/// Attaches a local terminal to a terminal another fleet host owns.
pub mod remote_terminal;
pub use st3_store::render;
pub use st3_store::resource;
pub use st3_store::rollout;
pub use st3_kernel::seat_queue;
/// Runs any CLI with credentials no seat can read, through a gateway the sekrets user owns.
pub use st3_store::sekrets;
pub use st3_store::sekrets_daemon;
pub mod service;
pub use st3_kernel::skill;
pub use st3_telemetry::slo;
pub use st3_telemetry::slo_history;
pub use st3_store::startup;
pub use st3_telemetry::startup_telemetry;
pub use st3_store::store;
pub mod subagents;
/// Suspends a quiet seat and resumes its own native session.
pub use st3_store::suspension;
pub(crate) use st3_kernel::publication;
pub mod seat_snapshot;
pub use st3_telemetry::telemetry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod kernel_tests;
#[cfg(test)]
mod store_tests;
#[cfg(test)]
mod telemetry_tests;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;
pub use st3_kernel::terminal_binding;
/// Runs each agent as its own Unix user, in its own namespaces and root-owned cgroup.
pub use st3_store::tile_registry;
pub mod tiles;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};

/// Shared authentication and transport for daemon GitHub callers.
pub(crate) use st3_store::github_http;
use st3_kernel::github_admission;
