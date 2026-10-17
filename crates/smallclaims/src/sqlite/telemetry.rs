//! Bounded SQLite metrics and request-local writer accounting. No SDK dependency.
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

static EXPORT_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Like the profiler's default, retain only batches whose wall time reaches 250 ms.
pub(super) const SLOW_BATCH: Duration = Duration::from_millis(250);

/// Arm writer roots only after the process trace provider is installed.
pub fn init(export_enabled: bool) {
    EXPORT_ENABLED.store(
        export_enabled && std::env::var("ST3_STORAGE_PHASE_SPANS").as_deref() == Ok("1"),
        Ordering::Relaxed,
    );
}

pub fn phase_spans_enabled() -> bool {
    EXPORT_ENABLED.load(Ordering::Relaxed)
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Origin(OriginLabel);

static TASK_ORIGINS: &[&str] = &[
    "other", "task checkpoint", "task migrate-event-payloads", "task expire-completed-responses",
    "task coordination-counts-catch-up", "task account-limits-catch-up",
    "task enforce-account-limits", "task trim-local-observations", "task mailbox-wake-dispatch",
    "task reconcile-pass", "task reconcile-deadline",
    "reconcile", "reconcile.timer", "reconcile.gate_poll",
    "published_list.agents", "published_list.missions", "published_list.work",
    "publication.attention", "publication.summary", "publication.advance_evidence",
    "replication.activate", "replication.retry_projection", "replication.export",
    "replication.receive", "replication.exchange", "replication.outbound",
    "startup.open_store", "startup.judge_claims", "startup.validate_replication_backlog",
    "startup.apply_replication_repairs", "startup.settle_runs",
    "startup.project_replication_backlog", "startup.warmup", "startup.isolation",
    "account_limits.enforce", "account_limits.catch_up", "maintenance.checkpoint",
    "maintenance.envelope_payloads", "maintenance.event_payloads",
    "maintenance.coordination_counts", "maintenance.stop_deferral_history",
    "maintenance.response_expiry", "maintenance.wal_checkpoint",
    "observations.trim", "observations.trim_usage_responses", "observations.database_size",
    "observations.export", "operations.repair_drift", "recorder.receipts",
    "mailbox.wake_dispatch", "mailbox.own_post", "mailbox.write", "mailbox.subscription",
    "message_body.reconcile", "seat.snapshot_transfer",
    "runtime.start", "sekrets.import", "changes.discover", "changes.sync",
    "account_limits.ready", "allocator.trim", "config.reload_policies",
    "github.auth", "github.request", "maintenance.malloc_relief",
    "maintenance.response_expiry_clock", "maintenance.response_expiry_deadline",
    "peer.raw_lease", "profile.runtime_lag", "publication.evidence",
    "publication.evidence_ready", "reconcile.supervise", "replication.connectivity",
    "replication.dial_peer", "replication.fabric_exposure", "replication.fabric_presence",
    "replication.fleet_view", "replication.peer_failure", "replication.publish_endpoints",
    "replication.settings", "replication.tailnet", "replication.worker",
    "slo.export", "slo.history", "terminal.socket", "terminal.tunnel",
    "terminal.view_dispatch", "terminal.view_watch", "tile.endpoint", "tile.endpoints",
    "writer.batch.setup", "writer.batch.job", "writer.batch.commit",
    "task replication-projection-retry", "startup validate-replication-backlog",
    "startup apply-replication-repairs", "startup project-replication-backlog",
];

#[derive(Clone, PartialEq, Eq, Hash)]
enum OriginLabel {
    // One pointer into the bounded table keeps queued jobs compact without allocating task labels.
    Static(&'static &'static str),
    Route(Arc<String>),
}

impl Origin {
    pub(crate) fn other() -> Self { Self(OriginLabel::Static(&TASK_ORIGINS[0])) }

    fn attribute(&self, key: &'static str) -> KeyValue {
        match &self.0 {
            OriginLabel::Static(label) => KeyValue::new(key, **label),
            OriginLabel::Route(label) => KeyValue::new(key, label.to_string()),
        }
    }

    pub(super) fn label(&self) -> &str {
        match &self.0 {
            OriginLabel::Static(label) => label,
            OriginLabel::Route(label) => label,
        }
    }
}

/// `route` must come from the router's normalized MatchedPath, never the request URI.
pub fn route_origin(method: &str, route: &str) -> Option<Origin> {
    if !matches!(method, "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" | "CONNECT" | "TRACE") {
        return Some(Origin::other());
    }
    Some(Origin(OriginLabel::Route(Arc::new(format!("{method} {route}")))))
}

fn task_origin(label: &'static str) -> Origin {
    let label = TASK_ORIGINS.iter().find(|bounded| **bounded == label).unwrap_or(&TASK_ORIGINS[0]);
    Origin(OriginLabel::Static(label))
}

thread_local! {
    static ORIGIN: RefCell<Option<Origin>> = const { RefCell::new(None) };
}

pub struct OriginScope(Option<Origin>);

pub fn enter_origin(origin: Option<Origin>) -> Option<OriginScope> {
    Some(OriginScope(ORIGIN.with(|slot| slot.replace(origin))))
}

pub(crate) fn enter_task_origin(label: &'static str) -> Option<OriginScope> {
    if ORIGIN.with(|slot| slot.borrow().is_some())
        || REQUEST.with(|slot| slot.borrow().as_ref().is_some_and(|request| request.origin.is_some())) {
        return None;
    }
    enter_origin(Some(task_origin(label)))
}

pub(crate) fn current_origin() -> Option<Origin> {
    Some(ORIGIN.with(|slot| slot.borrow().clone())
        .or_else(|| REQUEST.with(|slot| slot.borrow().as_ref().and_then(|request| request.origin.clone())))
        .unwrap_or_else(Origin::other))
}

impl Drop for OriginScope {
    fn drop(&mut self) {
        ORIGIN.with(|slot| slot.replace(self.0.take()));
    }
}

pub(super) struct Writer {
    holder: std::sync::Mutex<Option<Origin>>,
    current_hold: std::sync::Mutex<Option<(Origin, std::time::Instant)>>,
    checkpoint: std::sync::Mutex<Checkpoint>,
}

#[derive(Default)]
struct Checkpoint {
    active: usize,
    since: Option<std::time::Instant>,
    elapsed: Duration,
}

impl Checkpoint {
    fn sample(&mut self) -> Duration {
        let now = std::time::Instant::now();
        if self.active > 0 && let Some(since) = self.since {
            self.elapsed += now.duration_since(since);
        }
        self.since = Some(now);
        self.elapsed
    }
}

static WRITERS: LazyLock<std::sync::Mutex<std::collections::HashMap<String, std::sync::Weak<Writer>>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

static WRITER_OBSERVATIONS: std::sync::Mutex<Vec<std::sync::Weak<Writer>>> =
    std::sync::Mutex::new(Vec::new());
static HOLD_ORIGINS: LazyLock<std::sync::Mutex<std::collections::HashSet<Origin>>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::from([Origin::other()])));

impl Writer {
    pub(super) fn new(connection: &rusqlite::Connection) -> Option<Arc<Self>> {
        let writer = Arc::new(Self {
            holder: std::sync::Mutex::new(None),
            current_hold: std::sync::Mutex::new(None),
            checkpoint: std::sync::Mutex::new(Checkpoint::default()),
        });
        if let Some(path) = connection.path().filter(|path| !path.is_empty()) {
            let mut writers = WRITERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            writers.retain(|_, writer| writer.strong_count() > 0);
            writers.insert(path.to_owned(), Arc::downgrade(&writer));
        }
        let mut observations = WRITER_OBSERVATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        observations.retain(|writer| writer.strong_count() > 0);
        observations.push(Arc::downgrade(&writer));
        Some(writer)
    }

    pub(super) fn holder(&self) -> Option<Origin> {
        self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    pub(super) fn holding(&self, origin: Option<Origin>) -> Holding<'_> {
        let origin = origin.unwrap_or_else(Origin::other);
        HOLD_ORIGINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(origin.clone());
        *self.current_hold.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((origin.clone(), std::time::Instant::now()));
        self.set_holder(Some(origin));
        Holding(self)
    }

    pub(super) fn set_holder(&self, origin: Option<Origin>) {
        *self.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(origin.unwrap_or_else(Origin::other));
    }

    fn checkpoint_time(&self) -> Duration {
        self.checkpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner).sample()
    }
}

pub(super) struct Holding<'a>(&'a Writer);
impl Drop for Holding<'_> {
    fn drop(&mut self) {
        *self.0.holder.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if let Some((origin, started)) = self.0.current_hold.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            METRICS.hold.record(started.elapsed().as_secs_f64(), &writer_attributes(&origin));
        }
    }
}

pub(super) struct CheckpointGuard(Arc<Writer>);
impl CheckpointGuard {
    pub(super) fn start(connection: &rusqlite::Connection) -> Option<Self> {
        if !phase_spans_enabled() { return None; }
        let writer = WRITERS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(connection.path()?).and_then(std::sync::Weak::upgrade)?;
        {
            let mut checkpoint = writer.checkpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            checkpoint.sample();
            checkpoint.active += 1;
        }
        Some(Self(writer))
    }
}

impl Drop for CheckpointGuard {
    fn drop(&mut self) {
        let mut checkpoint = self.0.checkpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        checkpoint.sample();
        checkpoint.active -= 1;
    }
}

pub(super) struct Batch<'a> {
    started: std::time::SystemTime,
    writer: &'a Writer,
    checkpoint_started: Duration,
    pub wait: Duration,
    pub holder: Option<Origin>,
    pub begin: Duration,
    pub operations: Duration,
    pub commit: Duration,
    pub finalizers: Duration,
    pub slowest: Duration,
    pub origin: Origin,
}

impl<'a> Batch<'a> {
    pub(super) fn start(writer: &'a Writer) -> Self {
        Self {
            started: std::time::SystemTime::now(),
            writer,
            checkpoint_started: writer.checkpoint_time(),
            wait: Duration::ZERO,
            holder: None,
            begin: Duration::ZERO,
            operations: Duration::ZERO,
            commit: Duration::ZERO,
            finalizers: Duration::ZERO,
            slowest: Duration::ZERO,
            origin: Origin::other(),
        }
    }

    pub(super) fn finish(self, elapsed: Duration, post_commit: Duration, count: usize) {
        use opentelemetry::trace::{Span as _, Tracer as _};
        if elapsed < SLOW_BATCH && self.wait < SLOW_BATCH {
            return;
        }
        let tracer = opentelemetry::global::tracer("smallclaims.sqlite");
        let mut span = tracer.span_builder("st.db.writer.batch")
            .with_start_time(self.started)
            .with_attributes([
                KeyValue::new("span.label", "writer batch"),
                KeyValue::new("db.system.name", "sqlite"),
                KeyValue::new("st.writer.wait_ms", self.wait.as_secs_f64() * 1000.),
                self.holder.as_ref().map_or_else(|| KeyValue::new("st.writer.holder", "none"),
                    |holder| holder.attribute("st.writer.holder")),
                // Overlap is not additive: a dedicated checkpoint can run during any phase.
                KeyValue::new("st.db.writer.batch.checkpoint_overlap_ms",
                    self.writer.checkpoint_time().saturating_sub(self.checkpoint_started).as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.begin_ms", self.begin.as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.operations_ms", self.operations.as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.commit_ms", self.commit.as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.finalizers_ms", self.finalizers.as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.post_commit_ms", post_commit.as_secs_f64() * 1000.),
                KeyValue::new("st.db.writer.batch.ops", i64::try_from(count).unwrap_or(i64::MAX)),
                KeyValue::new("st.db.writer.batch.slowest_op_ms", self.slowest.as_secs_f64() * 1000.),
                self.origin.attribute("st.db.writer.batch.slowest_op_origin"),
            ])
            .start_with_context(&tracer, &opentelemetry::Context::new());
        span.end();
    }
}

pub(super) static SQLITE: LazyLock<[KeyValue; 1]> =
    LazyLock::new(|| [KeyValue::new("db.system.name", "sqlite")]);
static BATCHED: LazyLock<[KeyValue; 2]> = LazyLock::new(|| {
    [
        KeyValue::new("db.system.name", "sqlite"),
        KeyValue::new("db.operation.name", "write.batched"),
    ]
});
static LEND: LazyLock<[KeyValue; 2]> = LazyLock::new(|| {
    [
        KeyValue::new("db.system.name", "sqlite"),
        KeyValue::new("db.operation.name", "write.lend"),
    ]
});
static READ: LazyLock<[KeyValue; 2]> = LazyLock::new(|| {
    [
        KeyValue::new("db.system.name", "sqlite"),
        KeyValue::new("db.operation.name", "read"),
    ]
});
static CHECKPOINT: LazyLock<[KeyValue; 2]> = LazyLock::new(|| {
    [
        KeyValue::new("db.system.name", "sqlite"),
        KeyValue::new("db.operation.name", "checkpoint.truncate"),
    ]
});

pub(super) struct Instruments {
    operation: Histogram<f64>,
    pub queue: Histogram<f64>,
    hold: Histogram<f64>,
    pub batch_size: Histogram<u64>,
    pub commit: Histogram<f64>,
    pub batch: Histogram<f64>,
    read_wait: Histogram<f64>,
    pub readers_opened: Counter<u64>,
    checkpoint: Histogram<f64>,
    deferred: Counter<u64>,
}

fn duration(meter: &Meter, name: &'static str) -> Histogram<f64> {
    meter
        .f64_histogram(name)
        .with_unit("s")
        .with_boundaries(vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1., 2.5, 5., 10., 30., 60.,
        ])
        .build()
}

fn writer_duration(meter: &Meter, name: &'static str) -> Histogram<f64> {
    meter.f64_histogram(name).with_unit("s").with_boundaries(vec![
        0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1., 2.5, 5., 10., 15., 30.,
        60., 120., 300., 600.,
    ]).build()
}

fn writer_attributes(origin: &Origin) -> [KeyValue; 2] {
    [KeyValue::new("db.system.name", "sqlite"), origin.attribute("st.db.writer.origin")]
}

pub(super) fn queued(wait: Duration, origin: Option<&Origin>) {
    METRICS.queue.record(wait.as_secs_f64(), &writer_attributes(&origin.cloned().unwrap_or_else(Origin::other)));
}

impl Instruments {
    fn new(meter: &Meter) -> Self {
        register_gauges(meter);
        Self {
            operation: duration(meter, "db.client.operation.duration"),
            queue: writer_duration(meter, "st.db.writer.queue.duration"),
            hold: writer_duration(meter, "st.db.writer.hold"),
            batch_size: meter
                .u64_histogram("st.db.writer.batch.size")
                .with_unit("{job}")
                .with_boundaries(vec![1., 2., 4., 8., 16., 32., 64., 128., 256.])
                .build(),
            commit: duration(meter, "st.db.writer.commit.duration"),
            batch: duration(meter, "st.db.writer.batch.duration"),
            read_wait: duration(meter, "db.client.connection.wait_time"),
            readers_opened: meter
                .u64_counter("st.db.readers.opened")
                .with_unit("{connection}")
                .build(),
            checkpoint: duration(meter, "st.db.wal.checkpoint.duration"),
            deferred: meter.u64_counter("st.db.wal.checkpoint.deferred")
                .with_unit("{attempt}").build(),
        }
    }
}

pub(super) static METRICS: LazyLock<Instruments> =
    LazyLock::new(|| Instruments::new(&opentelemetry::global::meter("smallclaims.sqlite")));

pub(super) fn acknowledged(wait: Duration, batched: bool) {
    METRICS.operation.record(
        wait.as_secs_f64(),
        if batched {
            BATCHED.as_slice()
        } else {
            LEND.as_slice()
        },
    );
    REQUEST.with(|slot| {
        if let Some(request) = slot.borrow().as_ref() {
            request.nanos.fetch_add(
                wait.as_nanos().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
            request.ops.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub(super) fn read_waited(wait: Duration) {
    METRICS
        .read_wait
        .record(wait.as_secs_f64(), READ.as_slice());
}

/// Record the explicit backup checkpoint, including failed attempts.
pub fn checkpoint_finished(elapsed: Duration) {
    METRICS
        .checkpoint
        .record(elapsed.as_secs_f64(), CHECKPOINT.as_slice());
}

/// Shared only across blocking sections belonging to one request. Writes allocate nothing.
#[derive(Default)]
pub struct WriterWait {
    nanos: AtomicU64,
    ops: AtomicU64,
    origin: Option<Origin>,
}

impl WriterWait {
    pub fn with_origin(origin: Option<Origin>) -> Self {
        Self { origin, ..Self::default() }
    }

    pub fn totals(&self) -> (f64, u64) {
        (
            self.nanos.load(Ordering::Relaxed) as f64 / 1_000_000.,
            self.ops.load(Ordering::Relaxed),
        )
    }
}

thread_local! {
    static REQUEST: RefCell<Option<Arc<WriterWait>>> = const { RefCell::new(None) };
}

pub fn current_request() -> Option<Arc<WriterWait>> {
    REQUEST.with(|slot| slot.borrow().clone())
}

/// Restores the previous scope on unwind and prevents accounting leaking between pooled threads.
pub struct RequestScope(Option<Arc<WriterWait>>);

pub fn enter_request(request: Option<Arc<WriterWait>>) -> RequestScope {
    RequestScope(REQUEST.with(|slot| slot.replace(request)))
}

struct PoolObservation {
    counts: std::sync::Weak<super::ReaderCounts>,
    idle: std::sync::Weak<std::sync::Mutex<Vec<super::ReadConnection>>>,
    wal: Option<std::path::PathBuf>,
    id: u64,
    lag: u64,
    deferred_since: Option<std::time::Instant>,
}

static POOLS: std::sync::Mutex<Vec<PoolObservation>> = std::sync::Mutex::new(Vec::new());

pub(super) fn register_pool(pool: &super::ReadPool) {
    let wal = (!pool.shared_memory).then(|| {
        let mut path = pool.path.as_os_str().to_os_string();
        path.push("-wal");
        std::path::PathBuf::from(path)
    });
    let mut pools = POOLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pools.retain(|pool| pool.counts.strong_count() > 0);
    pools.push(PoolObservation {
        counts: Arc::downgrade(&pool.counts),
        idle: Arc::downgrade(&pool.idle),
        wal,
        id: pool.diagnostic_id,
        lag: 0,
        deferred_since: None,
    });
}

/// Called once for every physical checkpoint attempt. Errors leave the last known lag and
/// deferral start intact; they are not evidence that a pinned WAL recovered.
pub fn wal_checkpoint_observed(pool: &super::ReadPool, report: Option<&super::WalCheckpointReport>) {
    let Some(report) = report else { return };
    let deferred = report.frames > 0 && !report.recycled;
    if deferred {
        METRICS.deferred.add(1, SQLITE.as_slice());
    }
    let mut pools = POOLS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = pools.iter_mut().find(|state| state.id == pool.diagnostic_id) {
        state.lag = u64::try_from(report.frames.saturating_sub(report.backfilled)).unwrap_or(0);
        if deferred {
            state.deferred_since.get_or_insert_with(std::time::Instant::now);
        } else {
            state.deferred_since = None;
        }
    }
}

fn register_gauges(meter: &Meter) {
    meter.f64_observable_gauge("st.db.read.loan.oldest_age").with_unit("s")
        .with_callback(|observer| {
            for (origin, age) in super::read_lifetime::oldest_loans() {
                observer.observe(age.as_secs_f64(), &[
                    KeyValue::new("db.system.name", "sqlite"), origin.attribute("origin"),
                ]);
            }
        }).build();
    meter.u64_observable_gauge("st.db.wal.checkpoint.lag").with_unit("{frame}")
        .with_callback(|observer| {
            let pools = POOLS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            observer.observe(pools.iter().filter(|pool| pool.counts.strong_count() > 0)
                .map(|pool| pool.lag).sum(), SQLITE.as_slice());
        }).build();
    meter.f64_observable_gauge("st.db.wal.checkpoint.deferred_age").with_unit("s")
        .with_callback(|observer| {
            let pools = POOLS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let age = pools.iter().filter(|pool| pool.counts.strong_count() > 0)
                .filter_map(|pool| pool.deferred_since).map(|since| since.elapsed())
                .max().unwrap_or_default();
            observer.observe(age.as_secs_f64(), SQLITE.as_slice());
        }).build();
    meter.f64_observable_gauge("st.db.writer.current_hold")
        .with_unit("s")
        .with_callback(|observer| {
            let origins = HOLD_ORIGINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let writers = WRITER_OBSERVATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Emit zero for previously seen origins too, so a finished hold cannot leave a stale gauge.
            for origin in origins.iter() {
                let mut elapsed: f64 = 0.;
                for writer in writers.iter().filter_map(std::sync::Weak::upgrade) {
                    let hold = writer.current_hold.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some((held_origin, started)) = &*hold && held_origin == origin {
                        elapsed = elapsed.max(started.elapsed().as_secs_f64());
                    }
                }
                observer.observe(elapsed, &writer_attributes(origin));
            }
        })
        .build();
    meter
        .u64_observable_gauge("st.db.readers.open")
        .with_unit("{connection}")
        .with_callback(|observer| {
            let pools = POOLS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let open = pools
                .iter()
                .filter_map(|pool| pool.counts.upgrade())
                .map(|counts| counts.open.load(Ordering::Relaxed) as u64)
                .sum();
            observer.observe(open, SQLITE.as_slice());
        })
        .build();
    meter
        .u64_observable_gauge("st.db.readers.idle")
        .with_unit("{connection}")
        .with_callback(|observer| {
            let pools = POOLS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle = pools
                .iter()
                .filter_map(|pool| pool.idle.upgrade())
                .map(|idle| {
                    idle.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len() as u64
                })
                .sum();
            observer.observe(idle, SQLITE.as_slice());
        })
        .build();
    meter
        .u64_observable_gauge("st.db.wal.size")
        .with_unit("By")
        .with_callback(|observer| {
            let pools = POOLS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let bytes = pools
                .iter()
                .filter(|pool| pool.counts.strong_count() > 0)
                .filter_map(|pool| pool.wal.as_ref())
                .map(|path| std::fs::metadata(path).map_or(0, |metadata| metadata.len()))
                .sum();
            observer.observe(bytes, SQLITE.as_slice());
        })
        .build();
}

impl Drop for RequestScope {
    fn drop(&mut self) {
        REQUEST.with(|slot| slot.replace(self.0.take()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    #[test]
    fn trace_only_origin_follows_request_scopes() {
        if isolated("trace_only_origin_follows_request_scopes") { return; }
        init(true);
        assert!(!crate::profile::enabled());
        crate::profile::task("task checkpoint", || {
            assert_eq!(current_origin().unwrap().attribute("origin").value,
                opentelemetry::Value::from("task checkpoint"));
        });
        let request = Arc::new(WriterWait::with_origin(route_origin("POST", "/v1/mailbox/receipts")));
        std::thread::spawn(move || {
            let _scope = enter_request(Some(request));
            crate::profile::task("task checkpoint", || {
                assert_eq!(current_origin().unwrap().attribute("origin").value,
                    opentelemetry::Value::from("POST /v1/mailbox/receipts"));
            });
        }).join().unwrap();
        assert_eq!(current_origin().unwrap().attribute("origin").value,
            opentelemetry::Value::from("other"));
        for label in [
            "task checkpoint", "task migrate-event-payloads", "task expire-completed-responses",
            "task coordination-counts-catch-up", "task account-limits-catch-up",
            "task enforce-account-limits", "task trim-local-observations",
            "task mailbox-wake-dispatch", "task reconcile-pass", "task reconcile-deadline",
            "reconcile", "published_list.agents", "published_list.missions",
            "published_list.work", "replication.export", "replication.receive",
            "startup.warmup", "maintenance.wal_checkpoint",
            "task replication-projection-retry", "startup validate-replication-backlog",
            "startup apply-replication-repairs", "startup project-replication-backlog",
        ] {
            assert_eq!(task_origin(label).attribute("origin").value, opentelemetry::Value::from(label));
            crate::profile::task(label, || {
                assert_eq!(current_origin().unwrap().attribute("origin").value,
                    opentelemetry::Value::from(label));
            });
        }
        assert_eq!(task_origin("unknown").attribute("origin").value, opentelemetry::Value::from("other"));
    }

    // Global instruments initialize once. A separate test process keeps provider ordering
    // deterministic without serializing or changing providers underneath other store tests.
    fn isolated(name: &str) -> bool {
        if std::env::var("SMALLCLAIMS_TELEMETRY_TEST").as_deref() == Ok(name) {
            return false;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("sqlite::telemetry::tests::{name}"),
                "--nocapture",
            ])
            .env("SMALLCLAIMS_TELEMETRY_TEST", name)
            .env("ST3_STORAGE_PHASE_SPANS", "1")
            .status()
            .unwrap();
        assert!(status.success());
        true
    }

    fn write() {
        let writer = super::super::WriterConnection::new(
            rusqlite::Connection::open_in_memory().unwrap(),
            Arc::new(AtomicU64::new(0)),
        );
        writer.batched(|tx| tx.execute_batch("CREATE TABLE telemetry_test (value INTEGER); INSERT INTO telemetry_test VALUES (1);"))
            .unwrap().unwrap();
    }

    #[test]
    fn batched_write_exports_bounded_metrics() {
        if isolated("batched_write_exports_bounded_metrics") {
            return;
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let request = Arc::new(WriterWait::default());
        let _scope = enter_request(Some(request.clone()));
        write();
        assert_eq!(request.totals().1, 1);
        assert!(request.totals().0 >= 0.);
        provider.force_flush().unwrap();
        let finished = exporter.get_finished_metrics().unwrap();
        for name in [
            "db.client.operation.duration",
            "st.db.writer.queue.duration",
            "st.db.writer.batch.size",
            "st.db.writer.commit.duration",
            "st.db.writer.batch.duration",
            "st.db.writer.hold",
        ] {
            let metric = finished
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            let check_attributes = |attributes: Vec<&KeyValue>| {
                assert_eq!(
                    attributes.len(),
                    if matches!(name, "db.client.operation.duration" | "st.db.writer.queue.duration" | "st.db.writer.hold") {
                        2
                    } else {
                        1
                    }
                );
                assert!(
                    attributes
                        .iter()
                        .any(|attribute| attribute.key.as_str() == "db.system.name"
                            && attribute.value == opentelemetry::Value::from("sqlite"))
                );
                if matches!(name, "st.db.writer.queue.duration" | "st.db.writer.hold") {
                    assert!(attributes.iter().any(|attribute|
                        attribute.key.as_str() == "st.db.writer.origin"
                            && attribute.value == opentelemetry::Value::from("other")));
                }
                if name == "db.client.operation.duration" {
                    assert!(
                        attributes
                            .iter()
                            .any(|attribute| attribute.key.as_str() == "db.operation.name"
                                && attribute.value == opentelemetry::Value::from("write.batched"))
                    );
                }
            };
            match metric.data() {
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                    assert_eq!(metric.unit(), "s");
                    let points: Vec<_> = histogram.data_points().collect();
                    assert_eq!(points.len(), 1);
                    assert_eq!(points[0].count(), 1);
                    assert!(points[0].sum() >= 0.);
                    check_attributes(points[0].attributes().collect());
                }
                AggregatedMetrics::U64(MetricData::Histogram(histogram)) => {
                    assert_eq!(metric.unit(), "{job}");
                    let points: Vec<_> = histogram.data_points().collect();
                    assert_eq!(points.len(), 1);
                    assert_eq!(points[0].count(), 1);
                    assert_eq!(points[0].sum(), 1);
                    assert_eq!(
                        points[0].bounds().collect::<Vec<_>>(),
                        vec![1., 2., 4., 8., 16., 32., 64., 128., 256.]
                    );
                    check_attributes(points[0].attributes().collect());
                }
                _ => panic!("wrong histogram type for {name}"),
            }
        }
        provider.shutdown().unwrap();
    }

    #[test]
    fn loan_hold_exports_origin_and_current_hold_without_phase_spans() {
        if isolated("loan_hold_exports_origin_and_current_hold_without_phase_spans") { return; }
        init(false);
        assert!(!phase_spans_enabled());
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build()).build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let writer = super::super::WriterConnection::new(
            rusqlite::Connection::open_in_memory().unwrap(), Arc::new(AtomicU64::new(0)));
        let guard = crate::profile::task("task replication-projection-retry", || writer.write());
        std::thread::sleep(Duration::from_millis(2));
        provider.force_flush().unwrap();
        let origin_matches = |attributes: Vec<&KeyValue>| attributes.iter().any(|attribute|
            attribute.key.as_str() == "st.db.writer.origin"
                && attribute.value == opentelemetry::Value::from("task replication-projection-retry"));
        let current = |expected_held: bool| {
            let finished = exporter.get_finished_metrics().unwrap();
            let metric = finished.iter().rev().flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == "st.db.writer.current_hold").unwrap();
            assert_eq!(metric.unit(), "s");
            let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() else {
                panic!("current hold must be an f64 gauge");
            };
            let point = gauge.data_points().find(|point| origin_matches(point.attributes().collect())).unwrap();
            if expected_held { assert!(point.value() > 0.); } else { assert_eq!(point.value(), 0.); }
        };
        current(true);
        drop(guard);
        // The next completed batch fences the asynchronous connection return and hold recording.
        writer.batched(|_| Ok::<(), rusqlite::Error>(())).unwrap().unwrap();
        provider.force_flush().unwrap();
        current(false);
        let finished = exporter.get_finished_metrics().unwrap();
        for name in ["st.db.writer.hold", "st.db.writer.queue.duration"] {
            let metric = finished.iter().rev().flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics()).find(|metric| metric.name() == name).unwrap();
            assert_eq!(metric.unit(), "s");
            let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
                panic!("{name} must be an f64 histogram");
            };
            let point = histogram.data_points().find(|point| origin_matches(point.attributes().collect())).unwrap();
            assert_eq!(point.count(), 1);
            assert_eq!(point.bounds().last(), Some(600.));
            if name == "st.db.writer.hold" { assert!(point.sum() > 0.); }
        }
        provider.shutdown().unwrap();
    }

    #[test]
    fn failed_commit_records_no_commit_duration() {
        if isolated("failed_commit_records_no_commit_duration") {
            return;
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        // A deferred foreign key passes its INSERT inside the batch and fails only at COMMIT.
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE missing_parent(id INTEGER PRIMARY KEY);
                 CREATE TABLE orphan(id INTEGER PRIMARY KEY, parent INTEGER NOT NULL
                     REFERENCES missing_parent(id) DEFERRABLE INITIALLY DEFERRED);",
            )
            .unwrap();
        let writer = super::super::WriterConnection::new(
            connection,
            Arc::new(AtomicU64::new(0)),
        );
        let failed = writer
            .batched(|tx| tx.execute("INSERT INTO orphan (id, parent) VALUES (1, 99)", ()))
            .unwrap_err();
        assert!(
            failed.contains("FOREIGN KEY"),
            "the batch must fail at COMMIT on the deferred key, not earlier: {failed}"
        );
        // The failed batch rolled back, so the writer still answers the next write.
        let rolled_back: i64 = writer
            .batched(|tx| tx.query_row("SELECT COUNT(*) FROM orphan", [], |row| row.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(rolled_back, 0);
        provider.force_flush().unwrap();
        let finished = exporter.get_finished_metrics().unwrap();
        let histogram_count = |name: &str| -> u64 {
            let metric = finished
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("missing {name}"));
            let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() else {
                panic!("{name} is not an f64 histogram");
            };
            histogram.data_points().map(|point| point.count()).sum()
        };
        // Both batches ran, but only the second one's COMMIT landed.
        assert_eq!(histogram_count("st.db.writer.batch.duration"), 2);
        assert_eq!(histogram_count("st.db.writer.commit.duration"), 1);
        provider.shutdown().unwrap();
    }

    #[test]
    fn wal_gauge_reports_zero_when_the_file_is_missing() {
        if isolated("wal_gauge_reports_zero_when_the_file_is_missing") {
            return;
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("absent-wal.db");
        // A database no writer has put into WAL mode yet: an empty file is a valid empty
        // database, and its -wal sidecar does not exist.
        std::fs::write(&path, b"").unwrap();
        let pool = super::super::ReadPool::new(&path, false).unwrap();
        provider.force_flush().unwrap();
        let finished = exporter.get_finished_metrics().unwrap();
        let gauge = finished
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "st.db.wal.size")
            .expect("the WAL gauge exports");
        let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = gauge.data() else {
            panic!("the WAL gauge is a u64 gauge");
        };
        let points: Vec<_> = gauge.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value(), 0);
        drop(pool);
        provider.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn wal_gauge_uses_the_opened_target_not_a_file_alias_sidecar() {
        if isolated("wal_gauge_uses_the_opened_target_not_a_file_alias_sidecar") {
            return;
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.sqlite3");
        let writer = rusqlite::Connection::open(&target).unwrap();
        writer.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE wal_path_fixture(value INTEGER);
             INSERT INTO wal_path_fixture VALUES(1);",
        ).unwrap();
        let alias = root.path().join("alias.sqlite3");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let mut alias_wal = alias.as_os_str().to_os_string();
        alias_wal.push("-wal");
        std::fs::write(&alias_wal, b"alias decoy").unwrap();
        let mut target_wal = target.as_os_str().to_os_string();
        target_wal.push("-wal");
        let expected = std::fs::metadata(&target_wal).unwrap().len();
        assert!(expected > 11);
        let pool = super::super::ReadPool::new(&alias, false).unwrap();
        assert_eq!(pool.path, std::fs::canonicalize(&target).unwrap());
        let value: i64 = pool.get().query_row("SELECT value FROM wal_path_fixture", [], |row| row.get(0)).unwrap();
        assert_eq!(value, 1);
        provider.force_flush().unwrap();
        let finished = exporter.get_finished_metrics().unwrap();
        let gauge = finished.iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "st.db.wal.size")
            .expect("the WAL gauge exports");
        let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = gauge.data() else {
            panic!("the WAL gauge is a u64 gauge");
        };
        let points: Vec<_> = gauge.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value(), expected);
        assert_eq!(std::fs::read(alias_wal).unwrap(), b"alias decoy");
        drop(pool);
        drop(writer);
        provider.shutdown().unwrap();
    }

    #[test]
    fn write_without_provider_works() {
        if isolated("write_without_provider_works") {
            return;
        }
        write();
    }

    #[test]
    fn request_scope_restores_and_shares_blocking_accounting() {
        let request = Arc::new(WriterWait::default());
        {
            let _scope = enter_request(Some(request.clone()));
            let inherited = current_request();
            std::thread::spawn(move || {
                let _scope = enter_request(inherited);
                REQUEST.with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .unwrap()
                        .ops
                        .fetch_add(1, Ordering::Relaxed)
                });
            })
            .join()
            .unwrap();
            {
                let _nested = enter_request(None);
                assert!(current_request().is_none());
            }
            assert!(Arc::ptr_eq(&current_request().unwrap(), &request));
        }
        assert!(current_request().is_none());
        assert_eq!(request.totals().1, 1);
    }
}
