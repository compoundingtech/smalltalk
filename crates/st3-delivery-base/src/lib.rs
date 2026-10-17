//! the message-delivery leaf modules: blobs, message bodies, person requests and the mailbox fence, which name no store code. st3-daemon re-exports each module at its old path; everything below is re-exported at
//! this crate's root so the moved modules keep their `crate::X` paths.

// Test helpers widened for st3-daemon's tests are unused when only the feature is on.
#![cfg_attr(all(feature = "test-support", not(test)), allow(dead_code))]

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
#[allow(dead_code, unused_macros)]
mod ci_test_paths;

pub use st3_secrets::*;

pub mod blobs;
pub mod message_body;
pub mod person_request;
pub mod mailbox;
