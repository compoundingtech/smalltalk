//! st3 telemetry: the OpenTelemetry setup, the git/gh command recorder and the wake and startup
//! telemetry of the st3 library, which name no store, api or reconcile code. st3-daemon re-exports
//! each module at its old path.

pub mod incremental;
pub mod otel;
pub mod otel_client_class;
/// Wake and pass telemetry of the reconcile loop.
pub mod reconcile_telemetry;
/// Observes git and gh calls without changing their command behavior.
pub mod recorder;
/// Summarizes command recorder logs from one or more hosts.
pub mod recorder_report;
pub mod otlp;
pub mod relay_trace;
pub mod slo;
pub mod slo_history;
pub mod startup_telemetry;
pub mod telemetry;
