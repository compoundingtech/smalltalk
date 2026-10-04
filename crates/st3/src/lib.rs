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
pub mod lane;
pub mod mailbox;
pub mod mission;
pub mod model;
/// A driver relaunches its harness on the native session a suspended seat resumes.
pub mod native_resume;
pub mod otlp;
pub mod peer;
pub mod person_request;
pub mod pricing;
pub use smallclaims::{performance, profile};
pub mod projection;
pub mod reconcile;
pub mod rules;
/// Observes git and gh calls without changing their command behavior.
pub mod recorder;
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
pub mod service;
/// The st agent skill bundled in this binary and installed for each harness.
pub mod skill;
pub mod store;
pub mod subagents;
/// Suspends a quiet seat and resumes its own native session.
pub mod suspension;
pub mod telemetry;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};
