//! The daemon client, startup status, tile registry and the credential gateway (sekrets): the
//! modules the store and everything above it use, which name no store code. st3-store re-exports
//! them at its root, and st3-daemon at their old paths.

// Test helpers widened for st3-daemon's tests are unused when only the feature is on.
#![cfg_attr(all(feature = "test-support", not(test)), allow(dead_code))]

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
#[allow(dead_code, unused_macros)]
mod ci_test_paths;

pub use st3_kernel::*;
pub use st3_telemetry::*;
pub use smallclaims::{performance, profile, task_cpu};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub mod client;
pub mod sekrets;
pub mod startup;
pub mod tile_registry;
