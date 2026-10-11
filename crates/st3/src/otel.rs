//! Process telemetry uses blocking OTLP/HTTP-JSON exporters on SDK-owned threads.
//! Sampling is the collector's decision, not an in-process one: every unit exports
//! all spans with AlwaysOn (never parent-based, so an unsampled remote parent
//! cannot suppress export) and one sampling policy covers all producers
//! (Nathan's decision on #1607).
//! Hooks and timer-driven driver commands retain their daemon-mediated telemetry path.
//! Agent shells export OTEL_* settings, including OTEL_SERVICE_NAME=oh-my-pi:
//! respect per-signal opt-outs and retain correlation attributes, but always use
//! the st unit's service name and explicit resource identity.

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, SdkMeterProvider, Stream};
use opentelemetry_sdk::trace::{
    BatchConfig, BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::{Layer as _, SubscriberExt};

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
            Self::Daemon => "st-daemon",
            Self::ReplicationWorker => "st-replication-worker",
            Self::Cli => "st-cli",
        }
    }

    fn shutdown_timeout(&self) -> Duration {
        match self {
            Self::Cli => Duration::from_millis(50),
            Self::Daemon | Self::ReplicationWorker => Duration::from_secs(5),
        }
    }
}

const CLI_BACKOFF_SECONDS: u64 = 300;

/// Bounded span batch queue (O11Y-R18): worst case is the queue of SpanData plus the
/// exporter's in-flight OTLP/JSON batch, and the SDK's 2048/512 defaults alone measured
/// +60 MiB RSS at saturation. 256 queued spans with 256-span export batches keep the
/// daemon's export state within the +32 MiB budget — one in-flight batch of at most the
/// queue's spans — while exporting at most a quarter of the POSTs of a 64-span batch,
/// whose per-request HTTP overhead alone exceeded the +2% CPU budget at saturation.
/// `scheduled_delay` keeps the SDK default (and `OTEL_BSP_SCHEDULE_DELAY`): at
/// saturation batch fullness drives the cadence; at fleet load the 5 s delay holds.
/// A full queue drops spans, which the SDK reports through its internal diagnostics.
/// The BatchConfigBuilder setters override the environment, so the standard
/// `OTEL_BSP_MAX_*` variables are re-applied here explicitly.
fn span_batch_config() -> BatchConfig {
    BatchConfigBuilder::default()
        .with_max_queue_size(
            std::env::var("OTEL_BSP_MAX_QUEUE_SIZE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(256),
        )
        .with_max_export_batch_size(
            std::env::var("OTEL_BSP_MAX_EXPORT_BATCH_SIZE")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(256),
        )
        .build()
}

fn cli_backoff_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("st3/otel-cli-backoff"))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cli_backoff_active(path: &Path, now: u64) -> bool {
    // One bounded read; nonblocking/no-follow also tolerates a FIFO or racing symlink.
    // No lock, collector contact, or retry is allowed on this hot initialization path.
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    let mut value = String::new();
    if file.take(32).read_to_string(&mut value).is_err() {
        return false;
    }
    value
        .trim()
        .parse::<u64>()
        .is_ok_and(|failure| now < failure.saturating_add(CLI_BACKOFF_SECONDS))
}

fn record_cli_failure(path: &Path, now: u64) -> std::io::Result<()> {
    let Some(directory) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    writeln!(temporary, "{now}")?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
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

/// Process metrics belong only to the daemon and are sampled by the SDK reader.
fn register_daemon_memory_metrics(provider: &SdkMeterProvider) {
    use opentelemetry::metrics::MeterProvider as _;

    let meter = provider.meter("st3");
    #[cfg(target_os = "linux")]
    meter
        .u64_observable_gauge("st.process.memory.rss")
        .with_description("Resident process memory from /proc/self/statm")
        .with_unit("By")
        .with_callback(|observer| {
            if let Some(bytes) = linux_rss_bytes() {
                observer.observe(bytes, &[]);
            }
        })
        .build();

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    meter
        .u64_observable_gauge("st.allocator.heap")
        .with_description("glibc heap bytes by allocator state")
        .with_unit("By")
        .with_callback(|observer| {
            // SAFETY: glibc >= 2.33 provides this thread-safe snapshot. It takes
            // allocator arena locks but neither frees nor invalidates allocations.
            let heap = unsafe { libc::mallinfo2() };
            for (state, bytes) in [
                ("allocated", heap.uordblks),
                ("free", heap.fordblks),
                ("mmapped", heap.hblkhd),
                ("arena", heap.arena),
                ("releasable", heap.keepcost),
            ] {
                observer.observe(bytes as u64, &[KeyValue::new("state", state)]);
            }
        })
        .build();

    meter
        .u64_observable_counter("st.allocator.reclaim.count")
        .with_description("Checkpoint allocator reclamation attempts")
        .with_unit("{attempt}")
        .with_callback(|observer| {
            observer.observe(smallclaims::store::allocator_reclaim_stats().attempts, &[]);
        })
        .build();
    meter
        .f64_observable_counter("st.allocator.reclaim.duration")
        .with_description("Cumulative time spent reclaiming checkpoint allocator memory")
        .with_unit("s")
        .with_callback(|observer| {
            observer.observe(
                smallclaims::store::allocator_reclaim_stats().elapsed.as_secs_f64(), &[],
            );
        })
        .build();
}

#[cfg(target_os = "linux")]
fn linux_rss_bytes() -> Option<u64> {
    let mut buffer = [0_u8; 128];
    let bytes = std::fs::File::open("/proc/self/statm").ok()?.read(&mut buffer).ok()?;
    let pages = std::str::from_utf8(&buffer[..bytes]).ok()?
        .split_whitespace().nth(1)?.parse::<u64>().ok()?;
    // SAFETY: sysconf reads the process's system configuration; this constant
    // requires no pointer arguments. An unsupported/error return is not a sample.
    let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    pages.checked_mul(page_size)
}

pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    shutdown_timeout: Duration,
    cli: bool,
    cli_backoff: Option<PathBuf>,
}

impl Telemetry {
    pub fn init(unit: Unit, node: Option<&str>) -> Self {
        let mut telemetry = Self {
            tracer_provider: None,
            meter_provider: None,
            logger_provider: None,
            shutdown_timeout: unit.shutdown_timeout(),
            cli: matches!(unit, Unit::Cli),
            cli_backoff: None,
        };
        // These units previously installed no subscriber. Preserve that behavior when
        // export is disabled; only Driver and hook entrypoints own local diagnostics.
        if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none()
            || std::env::var("OTEL_SDK_DISABLED")
                .is_ok_and(|value| value.eq_ignore_ascii_case("true"))
            || (telemetry.cli && std::env::var("ST3_CLI_OTEL").as_deref() == Ok("off"))
        {
            return telemetry;
        }
        if telemetry.cli {
            telemetry.cli_backoff = cli_backoff_path();
            if telemetry
                .cli_backoff
                .as_deref()
                .is_some_and(|path| cli_backoff_active(path, unix_seconds()))
            {
                return telemetry;
            }
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
                    let provider = SdkTracerProvider::builder()
                        .with_sampler(Sampler::AlwaysOn)
                        .with_span_processor(
                            BatchSpanProcessor::builder(exporter)
                                .with_batch_config(span_batch_config())
                                .build(),
                        )
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
                    if matches!(unit, Unit::Daemon) {
                        register_daemon_memory_metrics(&provider);
                    }
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
        // The bridge exports INFO and above: per-request events (transport, protocol,
        // framework DEBUG) stay on stderr only, so a saturated daemon exports no log
        // stream, while real WARN/ERROR diagnostics still reach the collector.
        let export_filter = tracing_subscriber::EnvFilter::new(
            "info,opentelemetry=off,opentelemetry_sdk=off,opentelemetry_http=off,opentelemetry_otlp=off",
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
        let _ = shutdown_providers(
            tracer,
            meter,
            logger,
            self.shutdown_timeout,
            self.cli_backoff.clone(),
        );
    }
}

/// Keep both shutdown and provider destruction off the caller's deadline.
fn shutdown_providers(
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
    timeout: Duration,
    backoff: Option<PathBuf>,
) -> Result<(), std::sync::mpsc::RecvTimeoutError> {
    let deadline = Instant::now() + timeout;
    let helper_backoff = backoff.clone();
    let (done, waiting) = std::sync::mpsc::sync_channel(1);
    // SDK 0.30 returns OTelSdkResult from provider shutdown. BatchSpanProcessor
    // forwards the final export result; meter shutdown may ignore its timeout.
    // Keep shutdown AND destruction off the caller; a CLI export now always
    // has a root span, so its 50 ms bound applies to every flush.
    let _ = std::thread::spawn(move || {
        let mut failed = false;
        if let Some(provider) = tracer {
            failed |= provider
                .shutdown_with_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err();
        }
        if let Some(provider) = logger {
            failed |= provider
                .shutdown_with_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_err();
        }
        if let Some(provider) = meter {
            // Final collection is part of shutdown; force_flush would duplicate it.
            failed |= provider.shutdown().is_err();
        }
        if failed && let Some(path) = helper_backoff {
            let _ = record_cli_failure(&path, unix_seconds());
        }
        let _ = done.send(());
    });
    let result = waiting.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    if result.is_err()
        && let Some(path) = backoff
    {
        // The process can exit before the detached exporter wakes up.
        let _ = record_cli_failure(&path, unix_seconds());
    }
    result
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SignalChoice, Unit, build_resource, cli_backoff_active, record_cli_failure,
        shutdown_providers, signal_enabled, span_batch_config,
    };
    use opentelemetry::{Key, KeyValue, Value};
    use opentelemetry_sdk::Resource;

    #[derive(Debug)]
    struct StalledExporter(std::sync::mpsc::Sender<()>);

    impl opentelemetry_sdk::trace::SpanExporter for StalledExporter {
        async fn export(
            &self,
            _batch: Vec<opentelemetry_sdk::trace::SpanData>,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            std::future::pending().await
        }

        fn shutdown_with_timeout(
            &mut self,
            _timeout: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            self.0.send(()).unwrap();
            // Intentionally violate the exporter deadline: only the detached
            // helper's recv_timeout can bound this shutdown.
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    fn cli_shutdown_deadline_times_out_stalled_provider_and_records_backoff() {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::{Duration, Instant};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("st3/otel-cli-backoff");
        let (entered, stalled) = channel();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(StalledExporter(entered))
            .build();
        let deadline = Unit::Cli.shutdown_timeout();
        let started = Instant::now();
        let result = shutdown_providers(Some(provider), None, None, deadline, Some(path.clone()));
        let elapsed = started.elapsed();
        assert_eq!(result, Err(RecvTimeoutError::Timeout));
        // Allow 200 ms for shared-host scheduling and the atomic backoff write,
        // not exporter work. No whole-process baseline/startup enters this bound.
        assert!(
            elapsed <= deadline + Duration::from_millis(200),
            "{elapsed:?}"
        );
        stalled.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(path.is_file(), "timeout must establish the negative cache");
        assert!(cli_backoff_active(&path, super::unix_seconds()));
    }

    #[test]
    fn span_batch_config_bounds_export_memory() {
        // Debug is the only window into BatchConfig's crate-private fields; assert the
        // documented O11Y-R18 bounds so a dependency default change cannot slip in.
        let debug = format!("{:?}", span_batch_config());
        assert!(debug.contains("max_queue_size: 256"), "{debug}");
        assert!(debug.contains("max_export_batch_size: 256"), "{debug}");
    }

    #[test]
    fn cli_backoff_absent_proceeds() {
        let root = tempfile::tempdir().unwrap();
        assert!(!cli_backoff_active(&root.path().join("absent"), 1_000));
    }

    #[test]
    fn cli_backoff_fresh_skips_until_exact_expiry() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("st3/otel-cli-backoff");
        record_cli_failure(&path, 1_000).unwrap();
        assert!(cli_backoff_active(&path, 1_000));
        assert!(cli_backoff_active(&path, 1_299));
        assert!(!cli_backoff_active(&path, 1_300));
    }

    #[test]
    fn cli_backoff_stale_proceeds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("otel-cli-backoff");
        record_cli_failure(&path, 1_000).unwrap();
        assert!(!cli_backoff_active(&path, 1_301));
    }

    #[test]
    fn cli_backoff_corrupt_proceeds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("otel-cli-backoff");
        for value in [
            "",
            "not a timestamp",
            "-1",
            "1000 garbage",
            "18446744073709551616",
        ] {
            std::fs::write(&path, value).unwrap();
            assert!(!cli_backoff_active(&path, 1_000), "{value:?}");
        }
    }

    #[test]
    fn cli_backoff_write_atomically_replaces_previous_failure() {
        use std::io::Read as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("st3/otel-cli-backoff");
        record_cli_failure(&path, 1_000).unwrap();
        let mut old_reader = std::fs::File::open(&path).unwrap();
        record_cli_failure(&path, 2_000).unwrap();
        let mut old_value = String::new();
        old_reader.read_to_string(&mut old_value).unwrap();
        // Existing readers retain the whole previous inode, never a partial rewrite.
        assert_eq!(old_value, "1000\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "2000\n");
        assert!(cli_backoff_active(&path, 2_100));
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

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

    /// Opt-in cost proof: ~1 GiB in small, touched allocations, with alternate
    /// allocations freed so the arena retains fragmented free bins.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    #[ignore = "allocates a fragmented 1 GiB heap; run explicitly for allocator cost evidence"]
    fn mallinfo2_fragmented_heap_cost() {
        let mut allocations: Vec<_> = (0..262_144)
            .map(|_| Some(vec![0x5a_u8; 4096].into_boxed_slice()))
            .collect();
        for allocation in allocations.iter_mut().step_by(2) {
            *allocation = None;
        }
        std::hint::black_box(&allocations);
        // SAFETY: mallinfo2 is a thread-safe, read-only glibc allocator snapshot.
        let fragmented = unsafe { libc::mallinfo2() };
        assert!(fragmented.arena >= 1_073_741_824);
        assert!(fragmented.fordblks >= 500_000_000);
        let mut samples = [std::time::Duration::ZERO; 101];
        for sample in &mut samples {
            let started = std::time::Instant::now();
            // SAFETY: same read-only allocator snapshot as the production callback.
            std::hint::black_box(unsafe { libc::mallinfo2() });
            *sample = started.elapsed();
        }
        samples.sort_unstable();
        println!(
            "mallinfo2 fragmented heap: arena={} allocated={} free={} bytes; n={}; median={:?} p95={:?} max={:?}",
            fragmented.arena, fragmented.uordblks, fragmented.fordblks,
            samples.len(), samples[50], samples[95], samples[100],
        );
    }
}
