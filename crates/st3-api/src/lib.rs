//! the HTTP API: the daemon's routes, the client-v0 contract and the request-latency windows. st3-daemon re-exports each module at its old path; everything below is re-exported at
//! this crate's root so the moved modules keep their `crate::X` paths.

// Test helpers widened for st3-daemon's tests are unused when only the feature is on.
#![cfg_attr(all(feature = "test-support", not(test)), allow(dead_code))]

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
#[allow(dead_code, unused_macros)]
mod ci_test_paths;

pub use st3_reconcile::*;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod api;
pub mod claude_channel;
pub mod creation;
pub mod delivery_hold;
pub mod gate_check;
pub mod graph_watch_ivm;
pub mod objectives;
pub mod subagent_sessions;
