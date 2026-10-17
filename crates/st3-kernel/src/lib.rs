//! st3 kernel: the leaf modules of the st3 library (model, wire types, CLI output, configuration-free
//! helpers) that name no store, api or reconcile code. st3-daemon re-exports each module at its old path.

pub mod cli_output;
pub mod accounts;
pub mod archive;
pub mod boot;
pub mod config;
pub mod conversation_search;
pub mod disk;
/// Offline signed gate observations in immutable git notes refs.
pub mod evidence;
pub mod native_views;
pub mod conversation_header;
pub mod fleet;
/// Built-in gate kinds: what gates shelled out for most, answered by st itself.
pub mod gate_kinds;
/// What an `st` command run inside an exec gate reports about itself.
pub mod gate_report;
pub mod harness_events;
/// The lifecycle hook set st3 publishes beneath its own state directory.
pub mod hooks;
pub mod instance;
pub mod lane;
pub mod memory;
pub mod read_cache;
pub mod model;
pub mod pricing;
pub mod projection;
pub mod rules;
pub mod seat_queue;
/// The st agent skill bundled in this binary and installed for each harness.
pub mod skill;
pub mod publication;
pub mod terminal_binding;
pub mod github_admission;
