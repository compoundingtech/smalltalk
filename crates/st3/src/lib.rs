//! st3 claims graph, API, reconciliation, and CLI support.

pub mod accounts;
pub mod api;
pub mod archive;
pub mod backup;
pub mod blobs;
pub mod boot;
pub(crate) mod checkout;
pub mod claude_channel;
pub mod client;
pub mod config;
pub mod conversation_search;
pub mod creation;
pub mod repositories;
pub(crate) mod disk;
/// Answers the hooks an st3 seat's harness runs: `st driver-hook NAME`.
pub mod delivery_hold;
pub mod driver_hook;
pub mod environment;
pub(crate) mod external_sessions;
pub mod fleet;
/// `st missions check`: run a mission file's exec gates once, now, the way a run would.
pub mod gate_check;
/// Built-in gate kinds: what gates shelled out for most, answered by st itself.
pub mod gate_kinds;
/// What an `st` command run inside an exec gate reports about itself.
pub mod gate_report;
pub mod github_watch;
pub mod graph;
pub mod harness_events;
/// The lifecycle hook set st3 publishes beneath its own state directory.
pub mod hooks;
pub mod incremental;
/// Consent, installation and health checks for each discovered harness integration.
pub mod integrations;
pub mod lane;
pub mod mailbox;
/// Bounded local maintenance workers shared by daemon startup and lifecycle fixtures.
pub mod maintenance;
pub(crate) mod memory;
pub mod mission;
pub mod model;
/// A driver relaunches its harness on the native session a suspended seat resumes.
pub mod native_resume;
pub mod node_identity;
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
pub mod omp_ask_resume;
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
pub mod otlp;
pub mod peer;
pub mod placement;
pub mod person_request;
pub mod pricing;
pub use smallclaims::{performance, profile};
pub mod projection;
pub mod provenance;
pub mod reconcile;
pub mod rules;
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
pub mod seat_queue;
/// Runs any CLI with credentials no seat can read, through a gateway the sekrets user owns.
pub mod sekrets;
pub mod service;
/// First-run configuration and human-only daemon startup.
pub mod setup;
pub mod onboarding;
/// The st agent skill bundled in this binary and installed for each harness.
pub mod skill;
pub mod startup;
pub mod store;
pub mod subagents;
/// Suspends a quiet seat and resumes its own native session.
pub mod suspension;
pub mod seat_snapshot;
pub mod telemetry;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;
pub mod terminal_binding;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};

/// Shared authentication and transport for daemon GitHub callers.
mod github_http;
