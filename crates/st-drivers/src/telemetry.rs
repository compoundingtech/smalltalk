//! OpenTelemetry export for st2, per `docs/vrs/06-observability/`.
//!
//! Zero-overhead no-op unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set; the exporters then ship
//! OTLP/HTTP JSON to the fleet's local Alloy forwarder (normally `127.0.0.1:4318`). The process
//! model is sync (no tokio), so the blocking reqwest client is mandatory — enabling both
//! reqwest client features of `opentelemetry-otlp` 0.30 compiles but fails at runtime with
//! `NoHttpClient` (all client cfg arms exclude each other).
//!
//! Spans and logs share one `tracing` subscriber (interview decision Q6): a human-readable
//! stderr fmt layer runs unconditionally so diagnostics stay visible, and — behind the endpoint
//! guard — a `tracing-opentelemetry` layer exports spans while `opentelemetry-appender-tracing`
//! exports log records via an SDK logger provider. Logs emitted inside a span automatically
//! carry its trace/span ids.

use std::io::IsTerminal as _;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, SdkMeterProvider, Stream};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Layer as _, SubscriberExt};

/// Unlike `tracing::enabled!`, this tracks whether the process actually installed an OTLP
/// tracer layer. The stderr formatter exists without an endpoint, so hierarchy callsites must
/// use this guard before constructing child spans or computing their attributes.
static TRACER_EXPORT_ENABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn tracer_export_enabled() -> bool {
    TRACER_EXPORT_ENABLED.load(Ordering::Relaxed)
}

/// Seconds-scale explicit bucket boundaries for the duration histograms. The SDK's default
/// boundaries are millisecond-tuned (`[0, 5, 10, …, 10000]`), so sub-second reconcile passes
/// and session spawns would collapse into the lowest buckets and be indistinguishable.
const DURATION_BUCKET_BOUNDARIES: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// View mapping each duration instrument onto seconds-scale explicit buckets (see
/// [`DURATION_BUCKET_BOUNDARIES`]); every other instrument keeps its default aggregation.
fn duration_view(instrument: &Instrument) -> Option<Stream> {
    duration_instrument(instrument.name()).then(|| {
        Stream::builder()
            .with_aggregation(Aggregation::ExplicitBucketHistogram {
                boundaries: DURATION_BUCKET_BOUNDARIES.into(),
                record_min_max: true,
            })
            .build()
            .expect("duration histogram view is valid")
    })
}

fn duration_instrument(name: &str) -> bool {
    matches!(
        name,
        "reconcile_pass_duration_seconds"
            | "session_start_duration_seconds"
            | "resource_observe_dispatch_seconds"
            | "resource_observe_settle_seconds"
    )
}

/// Level filtering for the stderr fmt layer, defaulting to INFO. `RUST_LOG` overrides it on that
/// layer ONLY (see `install_subscriber`), so stderr verbosity can never silence OTel export.
fn default_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// Guard holding the tracer, meter, and logger providers for a process lifetime. Explicit
/// shutdown delivers pending telemetry from short-lived CLI invocations; the process-global
/// log bridge requires slightly different logger-provider lifetime handling (see [`Self::shutdown`]).
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
}

impl Telemetry {
    /// Diagnostics on stderr and nothing else: no exporters built, no global providers set, no
    /// endpoint consulted, and a `shutdown` with nothing to flush.
    ///
    /// For a process whose cadence is set by a harness's refresh timer rather than by an operator
    /// or an event. Claude's status-line tee is the case that motivated it: `refreshInterval: 5`
    /// makes it ~720 short-lived `st2` processes per hour per seat, and Claude WAITS for each one
    /// to exit, so the final collect-and-export [`Self::shutdown`] performs would sit in the
    /// render path — an unreachable collector turning a status line into a stall. A run of the tee
    /// is not an operation worth a span, so the honest fix is to not build the pipeline rather
    /// than to build it and hope the flush is quick.
    ///
    /// This is deliberately NOT "hook-class subcommands skip telemetry". `06-observability`'s
    /// spec instruments `st2 driver claude-observe` by name as `st2-hook` and records that other
    /// hook surfaces are not instrumented yet, so a blanket rule would drop an instrumented
    /// surface. The rule is about cadence, not about being a hook.
    pub fn local_only() -> Self {
        let _ = install_subscriber(tracing_subscriber::registry(), None, None, default_filter());
        Self {
            tracer_provider: None,
            meter_provider: None,
            logger_provider: None,
        }
    }

    /// Initialize telemetry for one process unit (`supervisor`, `cli`, ...). The service name
    /// follows the central observability contract's process-unit boundary: `st2-<unit>`.
    pub fn init(unit: &str) -> Self {
        // The stderr fmt layer is installed unconditionally — with or without an endpoint (see
        // `install_subscriber`). Migrated `tracing` diagnostics must stay visible exactly when
        // the old `eprintln!` calls were, so the unset-endpoint case keeps human-visible output
        // rather than PR1's literal zero-output behavior (documented deviation, spec.md "Log
        // bridge"). Level filtering defaults to INFO; `RUST_LOG` overrides it — on the stderr
        // fmt layer ONLY (see `install_subscriber`), so stderr verbosity can never silence
        // OTel span/log export (export-side sampling is a separate decision).
        let filter = default_filter();

        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none() {
            return Self::local_only();
        }

        let span_exporter = match build_span_exporter() {
            Ok(exporter) => exporter,
            // Export setup must never take the runner down: telemetry is best-effort.
            Err(err) => {
                eprintln!("st2: otel exporter unavailable, continuing without telemetry: {err}");
                return Self::local_only();
            }
        };

        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(format!("st2-{unit}"))
            .with_attribute(KeyValue::new(
                "service.version",
                crate::version::machine_version(),
            ))
            .with_attribute(KeyValue::new("host.name", crate::run::detect_host()))
            .build();

        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();
        opentelemetry::global::set_tracer_provider(tracer_provider.clone());
        let otel_span_layer = OpenTelemetryLayer::new(tracer_provider.tracer("st2"));

        // Metrics share endpoint, protocol, and resource with traces. Metric setup is
        // best-effort: if its exporter fails to build (e.g. malformed
        // `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`), metrics are disabled but span and log
        // export must continue. The periodic reader's default interval only governs
        // background collection — provider shutdown below performs the final collection, so
        // short-lived CLI runs still deliver their points.
        let meter_provider = match build_metric_exporter() {
            Ok(exporter) => {
                let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();
                let meter_provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_view(duration_view)
                    .with_resource(resource.clone())
                    .build();
                opentelemetry::global::set_meter_provider(meter_provider.clone());
                crate::metrics::set_enabled(true);
                Some(meter_provider)
            }
            Err(err) => {
                eprintln!("st2: otel metric exporter unavailable, metrics disabled: {err}");
                None
            }
        };

        // Log records share endpoint, protocol, and resource too; the appender bridge maps
        // `tracing` events onto OTLP logs and stamps the current span context onto them.
        let logger_provider = match build_log_exporter() {
            Ok(exporter) => Some(
                SdkLoggerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(resource)
                    .build(),
            ),
            Err(err) => {
                eprintln!("st2: otel log exporter unavailable, logs disabled: {err}");
                None
            }
        };

        let tracer_layer_installed = install_subscriber(
            tracing_subscriber::registry(),
            Some(otel_span_layer),
            logger_provider.as_ref(),
            filter,
        );
        TRACER_EXPORT_ENABLED.store(tracer_layer_installed, Ordering::Relaxed);

        Self {
            tracer_provider: Some(tracer_provider),
            meter_provider,
            logger_provider,
        }
    }

    /// Whether export is active (endpoint configured and exporters initialized).
    pub fn enabled(&self) -> bool {
        self.tracer_provider.is_some()
    }

    /// Deliver pending spans, metric points, and log records, then stop bounded exporter
    /// workers. Safe to call multiple times.
    pub fn shutdown(&mut self) {
        TRACER_EXPORT_ENABLED.store(false, Ordering::Relaxed);
        // `PeriodicReader::shutdown` performs a final collect-and-export itself. Calling
        // `force_flush` first would export the same cumulative counter and histogram snapshot
        // twice for every short-lived process.
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
            crate::metrics::set_enabled(false);
        }
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        // The OpenTelemetry log bridge is installed in the process-global tracing subscriber
        // and retains a logger from this provider. That subscriber cannot be uninstalled, so
        // shutting the provider down here exposes a stopped BatchLogProcessor to subsequent
        // events (including HTTP-client events produced by exporter shutdown). Force-flush last
        // to deliver the correlated completion log, then leave the provider alive through the
        // global bridge until process exit.
        if let Some(provider) = self.logger_provider.take() {
            let _ = provider.force_flush();
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Install the global subscriber once: stderr fmt (always), span exporter layer, and log-record
/// bridge behind the endpoint guard. A second `Telemetry::init` in the same process cannot
/// replace it (`set_global_default` errors), which is fine: init runs once per entrypoint.
fn install_subscriber<S>(
    base: S,
    span_layer: Option<OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>>,
    logger_provider: Option<&SdkLoggerProvider>,
    filter: tracing_subscriber::EnvFilter,
) -> bool
where
    S: tracing::Subscriber + Send + Sync + 'static,
    for<'a> S: tracing_subscriber::registry::LookupSpan<'a>,
{
    // The stderr layer's `EnvFilter` is scoped to that layer only: `RUST_LOG` is a
    // stderr-noise knob and can never silently disable OTLP export.
    //
    // The OpenTelemetry layers carry their own static filter, likewise independent of
    // `RUST_LOG`, that silences ONLY the exporters' own instrumentation targets. Those
    // cannot pass through the layers they feed: `opentelemetry_sdk`'s BatchLogProcessor
    // emits tracing events from inside its own `emit` (channel-full/shutdown notices), so
    // an event arriving via the log bridge would re-enter the same processor and recurse
    // until the export thread overflows its stack. App signals are exported unfiltered;
    // export-side sampling remains a separate decision.
    let tracer_layer_present = span_layer.is_some();
    let otel_filter = tracing_subscriber::EnvFilter::new(
        "trace,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_http=off,opentelemetry_otlp=off",
    );
    let installed = tracing::subscriber::set_global_default(
        base.with(span_layer.with_filter(otel_filter.clone()))
            .with(
                logger_provider
                    .map(OpenTelemetryTracingBridge::new)
                    .with_filter(otel_filter),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(std::io::stderr().is_terminal())
                    .with_writer(std::io::stderr)
                    .with_filter(filter),
            ),
    )
    .is_ok();
    tracer_layer_present && installed
}

/// OTLP/HTTP-JSON span exporter; blocking reqwest client only (module docs).
fn build_span_exporter() -> Result<SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
        .build()
}

/// OTLP/HTTP-JSON metric exporter; same wire and client constraints as traces.
fn build_metric_exporter() -> Result<MetricExporter, opentelemetry_otlp::ExporterBuildError> {
    MetricExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
        .build()
}

/// OTLP/HTTP-JSON log exporter; same wire and client constraints as traces/metrics.
fn build_log_exporter() -> Result<LogExporter, opentelemetry_otlp::ExporterBuildError> {
    LogExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_view_covers_every_duration_histogram() {
        for name in [
            "reconcile_pass_duration_seconds",
            "session_start_duration_seconds",
            "resource_observe_dispatch_seconds",
            "resource_observe_settle_seconds",
        ] {
            assert!(duration_instrument(name), "{name}");
        }
        assert!(!duration_instrument("resource_observe_requests_total"));
    }
}
