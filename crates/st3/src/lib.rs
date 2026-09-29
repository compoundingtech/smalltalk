//! st3 claims graph, API, reconciliation, and CLI support.

pub mod api;
pub mod archive;
pub mod boot;
pub(crate) mod checkout;
pub mod client;
pub mod config;
pub(crate) mod disk;
pub mod environment;
pub(crate) mod external_sessions;
pub mod fleet;
pub mod graph;
pub mod lane;
pub mod mission;
pub mod model;
pub mod otlp;
pub mod peer;
pub mod projection;
pub mod reconcile;
/// Observes git and gh calls without changing their command behavior.
pub mod recorder;
/// Summarizes command recorder logs from one or more hosts.
pub mod recorder_report;
/// Attaches a local terminal to a terminal another fleet host owns.
pub mod remote_terminal;
pub mod render;
pub mod resource;
pub mod seat_queue;
pub mod service;
/// The st agent skill bundled in this binary and installed for each harness.
pub mod skill;
pub mod store;
/// Attaches a local terminal to another fleet host's PTY session over Fabric, without st daemons.
pub mod terminal_fabric;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};
