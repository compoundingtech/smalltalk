//! Tests of st3-telemetry modules that need daemon code (the store, the graph, the API); they reach
//! the modules through the re-exports.

mod incremental;
mod otlp;
mod otlp_slo;
