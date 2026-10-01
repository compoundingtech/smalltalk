//! st3 claims graph, API, reconciliation, and CLI support.

pub mod api;
pub mod archive;
pub mod boot;
pub(crate) mod checkout;
pub mod claude_channel;
pub mod client;
pub mod config;
pub(crate) mod disk;
/// Answers the hooks an st3 seat's harness runs: `st driver-hook NAME`.
pub mod delivery_hold;
pub mod driver_hook;
pub mod environment;
pub(crate) mod external_sessions;
pub mod fleet;
pub mod graph;
/// The lifecycle hook set st3 publishes beneath its own state directory.
pub mod hooks;
pub mod lane;
pub mod mailbox;
pub mod mission;
pub mod model;
pub mod otlp;
pub mod peer;
pub use smallclaims::{performance, profile};
pub mod projection;
pub mod reconcile;
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
pub mod seat_queue;
pub mod service;
/// The st agent skill bundled in this binary and installed for each harness.
pub mod skill;
pub mod store;
pub mod telemetry;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};
