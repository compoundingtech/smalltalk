//! st3 store: the claims store, its domains (missions, mailbox, github, seats' rollout and suspension,
//! sekrets) and the modules they need. st3-daemon re-exports each module at its old path.

// Test helpers widened for st3-daemon's tests are unused when only the feature is on.
#![cfg_attr(all(feature = "test-support", not(test)), allow(dead_code))]

#[cfg(any(test, feature = "test-support"))]
#[macro_use]
#[path = "../../../scripts/ci-test-paths.rs"]
mod ci_test_paths;

pub use st3_kernel::cli_output;
// cli_output exports these macros at its crate root; the library exported them at its own.
pub use st3_kernel::{cli_print, cli_println};
pub use st3_kernel::{
    accounts, conversation_header, native_views, config, fleet, gate_kinds, github_admission, harness_events, hooks, lane, model, pricing, publication,
    seat_queue, terminal_binding,
};
pub use smallclaims::{performance, profile};
pub use st3_telemetry::{incremental, otel, otlp, recorder, reconcile_telemetry, slo};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;


pub mod backup;
pub mod checkout;
pub mod external_sessions;
pub mod recorder_receipts;
pub mod render;
pub mod blobs;
pub mod changes;
pub mod client;
pub mod client_values;
pub mod declaration_syntax;
pub mod environment;
pub mod gate_parse;
pub mod github_http;
pub mod github_watch;
pub mod graph;
pub mod mailbox;
pub mod message_body;
pub mod mission;
pub mod native_resume;
pub mod person_request;
pub mod provenance;
pub mod references;
pub mod resource;
pub mod rollout;
pub mod sekrets;
pub mod sekrets_daemon;
pub mod silent;
pub mod startup;
pub mod store;
pub mod suspension;
pub mod tile_registry;

pub use graph::{parse_intent, validate_mission_runtimes};
pub use model::{NormalizedIntent, St3Error};
