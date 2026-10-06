//! Process telemetry uses blocking OTLP/HTTP-JSON exporters on SDK-owned threads.
//! Hooks and timer-driven driver commands retain their daemon-mediated telemetry path.
//! Agent shells export OTEL_* settings, including OTEL_SERVICE_NAME=oh-my-pi:
//! respect per-signal opt-outs and retain correlation attributes, but always use
//! the st3 unit's service name and explicit resource identity.

use std::io::IsTerminal as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, SdkMeterProvider, Stream};
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Layer as _, SubscriberExt};

use crate::otel_sampler::{LocalRootTailSampler, TailSamplingConfig};

static EXPORT_ENABLED: AtomicBool = AtomicBool::new(false);
static INSTANCE_ID: OnceLock<String> = OnceLock::new();

pub fn export_enabled() -> bool {
    EXPORT_ENABLED.load(Ordering::Relaxed)
}

/// Shared by the SDK resource and the daemon's observations exporter.
pub fn service_instance_id() -> &'static str {
    INSTANCE_ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

pub enum Unit {
    Daemon,
    ReplicationWorker,
    Cli,
}

impl Unit {
    fn service_name(&self) -> &'static str {
        match self {
            Self::Daemon => "st3-daemon",
            Self::ReplicationWorker => "st3-replication-worker",
            Self::Cli => "st3-cli",
        }
    }

    fn shutdown_timeout(&self) -> Duration {
        match self {
            Self::Cli => Duration::from_millis(250),
            Self::Daemon | Self::ReplicationWorker => Duration::from_secs(5),
        }
    }
}

pub fn resource_attributes(unit_service_name: &'static str, node: Option<&str>) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new("service.name", unit_service_name),
        KeyValue::new("service.version", st_drivers::version::machine_version()),
        KeyValue::new("service.instance.id", service_instance_id()),
        KeyValue::new("host.name", st_drivers::run::detect_host()),
    ];
    if let Some(node) = node {
        attributes.push(KeyValue::new("st3.node", node.to_owned()));
    }
    attributes
}

fn build_resource(
    builder: opentelemetry_sdk::resource::ResourceBuilder,
    unit_service_name: &'static str,
    attributes: impl IntoIterator<Item = KeyValue>,
) -> opentelemetry_sdk::Resource {
    // SDK default detectors run in Resource::builder(); attributes added afterward
    // win on conflicts. Set service.name last so inherited agent identity never wins.
    builder
        .with_attributes(attributes)
        .with_service_name(unit_service_name)
        .build()
}

#[derive(Debug, PartialEq, Eq)]
enum SignalChoice {
    Enabled,
    Disabled,
    Unsupported,
}

fn signal_enabled(value: Option<&str>) -> SignalChoice {
    match value {
        None | Some("otlp") => SignalChoice::Enabled,
        Some("none") => SignalChoice::Disabled,
        Some(_) => SignalChoice::Unsupported,
    }
}

fn signal_export_enabled(variable: &str) -> bool {
    let value = std::env::var(variable);
    let choice = match &value {
        Ok(value) => signal_enabled(Some(value)),
        Err(std::env::VarError::NotPresent) => signal_enabled(None),
        Err(std::env::VarError::NotUnicode(_)) => SignalChoice::Unsupported,
    };
    match choice {
        SignalChoice::Enabled => true,
        SignalChoice::Disabled => false,
        SignalChoice::Unsupported => {
            eprintln!("st: unsupported {variable}; signal disabled (expected otlp or none)");
            false
        }
    }
}

const DURATION_BUCKET_BOUNDARIES: [f64; 14] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

fn duration_view(instrument: &Instrument) -> Option<Stream> {
    (instrument.unit() == "s"
        && instrument.kind() == opentelemetry_sdk::metrics::InstrumentKind::Histogram)
        .then(|| {
            Stream::builder()
                .with_aggregation(Aggregation::ExplicitBucketHistogram {
                    boundaries: DURATION_BUCKET_BOUNDARIES.into(),
                    record_min_max: true,
                })
                .build()
                .expect("seconds histogram view is valid")
        })
}

pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Duration,
}

impl Telemetry {
    pub fn init(unit: Unit, node: Option<&str>) -> Self {
        let mut telemetry = Self {
            tracer_provider: None,
            meter_provider: None,
            logger_provider: None,
            shutdown_timeout: unit.shutdown_timeout(),
        };
        // These units previously installed no subscriber. Preserve that behavior when
        // export is disabled; only Driver and hook entrypoints own local diagnostics.
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none() {
            return telemetry;
        }
        let traces_enabled = signal_export_enabled("OTEL_TRACES_EXPORTER");
        let metrics_enabled = signal_export_enabled("OTEL_METRICS_EXPORTER");
        let logs_enabled = signal_export_enabled("OTEL_LOGS_EXPORTER");
        if !traces_enabled && !metrics_enabled && !logs_enabled {
            return telemetry;
        }
        let resource = build_resource(
            opentelemetry_sdk::Resource::builder(),
            unit.service_name(),
            resource_attributes(unit.service_name(), node),
        );
        if traces_enabled {
            match SpanExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
                .build()
            {
                Ok(exporter) => {
                    let config = TailSamplingConfig::default();
                    let provider = SdkTracerProvider::builder()
                        .with_sampler(crate::otel_sampler::head_sampler(&config))
                        .with_span_processor(LocalRootTailSampler::new(
                            BatchSpanProcessor::builder(exporter).build(),
                            config,
                        ))
                        .with_resource(resource.clone())
                        .build();
                    opentelemetry::global::set_tracer_provider(provider.clone());
                    telemetry.tracer_provider = Some(provider);
                }
                Err(error) => {
                    eprintln!("st: otel span exporter unavailable, traces disabled: {error}");
                }
            }
        }
        if metrics_enabled {
            match MetricExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
                .build()
            {
                Ok(exporter) => {
                    let provider = SdkMeterProvider::builder()
                        .with_reader(
                            opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build(),
                        )
                        .with_view(duration_view)
                        .with_resource(resource.clone())
                        .build();
                    opentelemetry::global::set_meter_provider(provider.clone());
                    telemetry.meter_provider = Some(provider);
                }
                Err(error) => {
                    eprintln!("st: otel metric exporter unavailable, metrics disabled: {error}");
                }
            }
        }
        if logs_enabled {
            match LogExporter::builder()
                .with_http()
                .with_protocol(opentelemetry_otlp::Protocol::HttpJson)
                .build()
            {
                Ok(exporter) => {
                    telemetry.logger_provider = Some(
                        SdkLoggerProvider::builder()
                            .with_batch_exporter(exporter)
                            .with_resource(resource)
                            .build(),
                    );
                }
                Err(error) => {
                    eprintln!("st: otel log exporter unavailable, logs disabled: {error}")
                }
            }
        }
        if telemetry.tracer_provider.is_none()
            && telemetry.meter_provider.is_none()
            && telemetry.logger_provider.is_none()
        {
            return telemetry;
        }
        let span_layer = telemetry
            .tracer_provider
            .as_ref()
            .map(|provider| OpenTelemetryLayer::new(provider.tracer("st3")));
        // SDK processor diagnostics must never feed the log bridge that produced them.
        // RUST_LOG affects stderr only, independently of exported application signals.
        let export_filter = tracing_subscriber::EnvFilter::new(
            "trace,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_http=off,opentelemetry_otlp=off",
        );
        let stderr_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let installed = tracing::subscriber::set_global_default(
            tracing_subscriber::registry()
                .with(span_layer.with_filter(export_filter.clone()))
                .with(
                    telemetry
                        .logger_provider
                        .as_ref()
                        .map(OpenTelemetryTracingBridge::new)
                        .with_filter(export_filter),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(std::io::stderr().is_terminal())
                        .with_writer(std::io::stderr)
                        .with_filter(stderr_filter),
                ),
        )
        .is_ok();
        EXPORT_ENABLED.store(
            installed && telemetry.tracer_provider.is_some(),
            Ordering::Relaxed,
        );
        telemetry
    }

    pub fn enabled(&self) -> bool {
        self.tracer_provider.is_some() && export_enabled()
    }

    /// One deadline covers every signal, including provider destruction.
    pub fn shutdown(&mut self) {
        EXPORT_ENABLED.store(false, Ordering::Relaxed);
        let tracer = self.tracer_provider.take();
        let meter = self.meter_provider.take();
        let logger = self.logger_provider.take();
        if tracer.is_none() && meter.is_none() && logger.is_none() {
            return;
        }
        let deadline = Instant::now() + self.shutdown_timeout;
        let (done, waiting) = std::sync::mpsc::sync_channel(1);
        // SDK 0.30's meter provider and PeriodicReader accept but IGNORE their timeout
        // (meter_provider.rs:113,139; periodic_reader.rs:495). force_flush has no timeout.
        // Keep all shutdown and Drop work off the caller; a timed-out thread is detached,
        // never joined, so a stalled collector cannot extend the process-exit deadline.
        let _ = std::thread::spawn(move || {
            if let Some(provider) = tracer {
                let _ = provider
                    .shutdown_with_timeout(deadline.saturating_duration_since(Instant::now()));
            }
            if let Some(provider) = logger {
                let _ = provider
                    .shutdown_with_timeout(deadline.saturating_duration_since(Instant::now()));
            }
            if let Some(provider) = meter {
                // Final collection is part of shutdown; force_flush would duplicate it.
                let _ = provider.shutdown();
            }
            let _ = done.send(());
        });
        let _ = waiting.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{SignalChoice, Unit, build_resource, signal_enabled};
    use opentelemetry::{Key, KeyValue, Value};
    use opentelemetry_sdk::Resource;

    #[test]
    fn signal_exporter_choices() {
        assert_eq!(signal_enabled(None), SignalChoice::Enabled);
        assert_eq!(signal_enabled(Some("otlp")), SignalChoice::Enabled);
        assert_eq!(signal_enabled(Some("none")), SignalChoice::Disabled);
        for value in [
            "",
            "console",
            "prometheus",
            "otlp,console",
            "NONE",
            " otlp ",
        ] {
            assert_eq!(signal_enabled(Some(value)), SignalChoice::Unsupported);
        }
    }

    #[test]
    fn explicit_resource_identity_overrides_inherited_agent_attributes() {
        for unit in [Unit::Daemon, Unit::ReplicationWorker, Unit::Cli] {
            // Inject detector output instead of changing process-wide OTEL_* variables.
            let inherited = Resource::builder_empty().with_attributes([
                KeyValue::new("service.name", "oh-my-pi"),
                KeyValue::new("service.version", "agent-version"),
                KeyValue::new("service.instance.id", "agent-instance"),
                KeyValue::new("host.name", "agent-host"),
                KeyValue::new("st3.node", "agent-node"),
                KeyValue::new("agent.actor.id", "actor-123"),
            ]);
            let resource = build_resource(
                inherited,
                unit.service_name(),
                [
                    KeyValue::new("service.name", "conflicting-attribute"),
                    KeyValue::new("service.version", "st3-version"),
                    KeyValue::new("service.instance.id", "st3-instance"),
                    KeyValue::new("host.name", "st3-host"),
                    KeyValue::new("st3.node", "st3-node"),
                ],
            );
            for (key, expected) in [
                ("service.name", unit.service_name()),
                ("service.version", "st3-version"),
                ("service.instance.id", "st3-instance"),
                ("host.name", "st3-host"),
                ("st3.node", "st3-node"),
                ("agent.actor.id", "actor-123"),
            ] {
                assert_eq!(resource.get(&Key::new(key)), Some(Value::from(expected)));
            }
        }
    }
}
