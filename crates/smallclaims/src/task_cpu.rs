//! Thread CPU consumed by daemon tasks, excluding time parked between polls.
use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

struct Instruments {
    cpu: Counter<f64>,
    polls: Counter<u64>,
}

static METRICS: LazyLock<Instruments> =
    LazyLock::new(|| instruments(&opentelemetry::global::meter("st3")));

static ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static RECORDER_ENTRIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Enable task metrics only after the process metrics provider is installed.
pub fn init(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

fn instruments(meter: &opentelemetry::metrics::Meter) -> Instruments {
    #[cfg(test)]
    RECORDER_ENTRIES.fetch_add(1, Ordering::Relaxed);
    Instruments {
        cpu: meter.f64_counter("st.task.cpu_time")
            .with_description("Thread CPU consumed while polling a daemon task or running its blocking work")
            .with_unit("s").build(),
        polls: meter.u64_counter("st.task.polls")
            .with_description("Daemon task polls and blocking invocations")
            .with_unit("{poll}").build(),
    }
}

fn thread_cpu() -> Option<Duration> {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: time is writable and the clock measures only the calling thread.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    Some(Duration::new(time.tv_sec.try_into().ok()?, time.tv_nsec.try_into().ok()?))
}

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
}

/// One accumulator travels with its future, even if Tokio changes worker threads.
struct Accumulator<'a> {
    cpu: Duration,
    polls: u64,
    attributes: [KeyValue; 1],
    metrics: &'a Instruments,
    flushed: Instant,
}

impl<'a> Accumulator<'a> {
    fn new(name: &'static str, metrics: &'a Instruments) -> Self {
        Self { cpu: Duration::ZERO, polls: 0, attributes: [KeyValue::new("st.task", name)],
            metrics, flushed: Instant::now() }
    }

    fn flush(&mut self) {
        if self.polls == 0 {
            return;
        }
        self.metrics.polls.add(self.polls, &self.attributes);
        if !self.cpu.is_zero() {
            self.metrics.cpu.add(self.cpu.as_secs_f64(), &self.attributes);
        }
        self.cpu = Duration::ZERO;
        self.polls = 0;
        self.flushed = Instant::now();
    }
}

impl Drop for Accumulator<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

struct Sampling {
    started: Option<Duration>,
    nested: bool,
}

impl Sampling {
    fn start() -> Self {
        #[cfg(test)]
        RECORDER_ENTRIES.fetch_add(1, Ordering::Relaxed);
        let nested = MEASURING.with(|active| active.replace(true));
        Self { started: (!nested).then(thread_cpu).flatten(), nested }
    }

    fn finish(&self, accumulator: &mut Accumulator<'_>) {
        let ended = (!self.nested).then(thread_cpu).flatten();
        MEASURING.with(|active| active.set(self.nested));
        accumulator.polls += 1;
        if let Some(elapsed) = self.started.zip(ended).and_then(|(start, end)| end.checked_sub(start)) {
            accumulator.cpu += elapsed;
        }
        if accumulator.flushed.elapsed() >= Duration::from_secs(1) {
            accumulator.flush();
        }
    }
}

struct Sample<'a, 'm> {
    sampling: Sampling,
    accumulator: &'a mut Accumulator<'m>,
}

impl<'a, 'm> Sample<'a, 'm> {
    fn start(accumulator: &'a mut Accumulator<'m>) -> Self {
        Self { sampling: Sampling::start(), accumulator }
    }
}

impl Drop for Sample<'_, '_> {
    fn drop(&mut self) {
        self.sampling.finish(self.accumulator);
    }
}

/// Measure each poll on its current thread; never retain a clock sample across an await.
/// The name must describe an operation, never an instance, seat, peer, or request.
/// Samples accumulate with the future and reach OTel at most once a second while
/// polling, plus a final flush on completion or cancellation. Nested wrappers on
/// the same thread count polls but leave their CPU to the outermost wrapper.
pub async fn account<F: Future>(name: &'static str, future: F) -> F::Output {
    if !ENABLED.load(Ordering::Relaxed) {
        return future.await;
    }
    measured(name, future, &METRICS).await
}

async fn measured<F: Future>(name: &'static str, future: F, metrics: &Instruments) -> F::Output {
    let mut accumulator = Accumulator::new(name, metrics);
    let mut future = pin!(future);
    poll_fn(|cx| {
        let _sample = Sample::start(&mut accumulator);
        let _origin = crate::sqlite::telemetry::enter_task_origin(name);
        crate::profile::profiled_task(name, || future.as_mut().poll(cx))
    }).await
}

/// Measure one blocking invocation, and label any writer work it performs.
pub fn blocking<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Relaxed) {
        return work();
    }
    let _scope = enabled_blocking_scope(name);
    work()
}

/// Add metrics while retaining an existing synchronous profile/performance scope.
pub fn profiled_blocking<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Relaxed) {
        return crate::profile::task(name, work);
    }
    let _scope = enabled_blocking_scope(name);
    work()
}

/// Add metrics while retaining a pre-existing performance-only callback scope.
pub fn performance_blocking<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    if !ENABLED.load(Ordering::Relaxed) {
        return crate::performance::task(name, work);
    }
    let _scope = enabled_blocking_scope(name);
    work()
}

/// Lexical blocking accounting for code whose sequential borrows cannot share a closure.
/// Synchronous and !Send: thread-clock samples and TLS must end on the entering thread.
pub(crate) struct BlockingScope {
    profile: Option<crate::profile::TaskScope>,
    sampling: Option<Sampling>,
    accumulator: Option<Accumulator<'static>>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub(crate) fn blocking_scope(name: &'static str) -> BlockingScope {
    if !ENABLED.load(Ordering::Relaxed) {
        return BlockingScope {
            profile: None,
            sampling: None,
            accumulator: None,
            _thread: std::marker::PhantomData,
        };
    }
    enabled_blocking_scope(name)
}

fn enabled_blocking_scope(name: &'static str) -> BlockingScope {
    let accumulator = Accumulator::new(name, &METRICS);
    let sampling = Sampling::start();
    let mut scope = BlockingScope {
        profile: None,
        sampling: Some(sampling),
        accumulator: Some(accumulator),
        _thread: std::marker::PhantomData,
    };
    // Install the sampling guard before profile entry, including its unwind path.
    scope.profile = Some(crate::profile::task_scope(name));
    scope
}

impl Drop for BlockingScope {
    fn drop(&mut self) {
        let Some(sampling) = self.sampling.take() else { return };
        // Keep a Sample guard alive while profiling exits, so even an exit panic
        // restores sampling TLS. Its drop precedes the accumulator's OTel flush.
        let _sample = Sample {
            sampling,
            accumulator: self.accumulator.as_mut().expect("enabled scope accumulator"),
        };
        drop(self.profile.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

    #[tokio::test]
    async fn disabled_task_metrics_do_not_enter_recorder() {
        const CHILD: &str = "SMALLCLAIMS_DISABLED_TASK_METRICS_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "task_cpu::tests::disabled_task_metrics_do_not_enter_recorder"])
                .env(CHILD, "1").env_remove("ST3_PROFILE_DIR").status().unwrap();
            assert!(status.success());
            return;
        }
        init(false);
        let entries = RECORDER_ENTRIES.load(Ordering::Relaxed);
        account("test.disabled.async", async {
            assert!(!MEASURING.with(Cell::get));
        }).await;
        blocking("test.disabled.blocking", || {
            assert!(!MEASURING.with(Cell::get));
            assert!(crate::performance::current().is_none());
            assert!(crate::profile::current().is_none());
        });
        profiled_blocking("test.baseline.profile", || {
            assert!(!MEASURING.with(Cell::get));
            assert_eq!(crate::performance::current().unwrap().kind(), "test.baseline.profile");
        });
        performance_blocking("test.baseline.performance", || {
            assert!(!MEASURING.with(Cell::get));
            assert_eq!(crate::performance::current().unwrap().kind(), "test.baseline.performance");
        });
        {
            let scope = blocking_scope("test.disabled.lexical");
            assert!(scope.accumulator.is_none());
            assert!(scope.sampling.is_none());
            assert!(scope.profile.is_none());
        }
        assert_eq!(RECORDER_ENTRIES.load(Ordering::Relaxed), entries);
    }

    #[test]
    fn lexical_and_callback_scopes_match_on_success_nesting_and_unwind() {
        const CHILD: &str = "ST3_BLOCKING_SCOPE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact",
                    "task_cpu::tests::lexical_and_callback_scopes_match_on_success_nesting_and_unwind",
                    "--nocapture"])
                .env(CHILD, "1")
                .env("ST3_PROFILE_DIR", directory.path())
                .env("ST3_PROFILE_SLOW_MS", "0")
                .status().unwrap();
            assert!(status.success());
            return;
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build()).build();
        opentelemetry::global::set_meter_provider(provider.clone());
        init(true);
        crate::profile::init_from_env();
        for (lexical, name) in [(false, "test.callback"), (true, "test.lexical"),
            (false, "test.callback.panic"), (true, "test.lexical.panic")] {
            let panics = name.ends_with(".panic");
            let work = || {
                assert!(MEASURING.with(Cell::get));
                assert_eq!(crate::performance::current().unwrap().kind(), name);
                assert!(crate::profile::current().is_some());
                {
                    let nested = blocking_scope("test.nested");
                    assert!(nested.sampling.as_ref().unwrap().nested);
                    assert_eq!(crate::performance::current().unwrap().kind(), name);
                }
                assert!(MEASURING.with(Cell::get), "nested exit must retain outer scope");
                if panics { panic!("exercise scope unwind"); }
                42
            };
            let result = std::panic::catch_unwind(|| {
                if lexical {
                    let scope = blocking_scope(name);
                    assert!(!scope.sampling.as_ref().unwrap().nested);
                    work()
                } else {
                    blocking(name, work)
                }
            });
            assert_eq!(result.is_err(), panics);
            if let Ok(value) = result { assert_eq!(value, 42); }
            assert!(!MEASURING.with(Cell::get));
            assert!(crate::performance::current().is_none());
            assert!(crate::profile::current().is_none());
        }
        // Neither entry form creates a new profile operation inside an existing one.
        let existing = crate::profile::Op::start("test.existing", None).unwrap();
        {
            let _entered = existing.enter();
            blocking("test.callback.existing", || ());
            let _scope = blocking_scope("test.lexical.existing");
        }
        existing.finish();
        provider.force_flush().unwrap();
        let exported = exporter.get_finished_metrics().unwrap();
        let mut polls = std::collections::BTreeMap::new();
        let mut cpu = std::collections::BTreeMap::new();
        for metric in exported.iter().flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics()) {
            match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) if metric.name() == "st.task.polls" => {
                    for point in sum.data_points() {
                        let task = point.attributes().find(|attribute| attribute.key.as_str() == "st.task").unwrap();
                        *polls.entry(task.value.to_string()).or_insert(0) += point.value();
                    }
                }
                AggregatedMetrics::F64(MetricData::Sum(sum)) if metric.name() == "st.task.cpu_time" => {
                    for point in sum.data_points() {
                        let task = point.attributes().find(|attribute| attribute.key.as_str() == "st.task").unwrap();
                        *cpu.entry(task.value.to_string()).or_insert(0.0) += point.value();
                    }
                }
                _ => {}
            }
        }
        for name in ["test.callback", "test.lexical", "test.callback.panic", "test.lexical.panic"] {
            assert_eq!(polls[name], 1, "one CPU invocation, including unwind");
        }
        assert_eq!(polls["test.nested"], 4);
        assert_eq!(cpu.get("test.nested").copied().unwrap_or(0.0), 0.0);
        let slow = std::path::PathBuf::from(std::env::var_os("ST3_PROFILE_DIR").unwrap()).join("slow.jsonl");
        let records = std::fs::read_to_string(slow).unwrap().lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
        for name in ["test.callback", "test.lexical", "test.callback.panic", "test.lexical.panic"] {
            let own = records.iter().filter(|record| record["label"] == name).collect::<Vec<_>>();
            assert_eq!(own.len(), 1);
            assert_eq!(own[0]["completion"], if name.ends_with(".panic") { "dropped" } else { "finished" });
        }
        assert_eq!(records.iter().filter(|record| record["label"] == "test.existing").count(), 1);
        assert!(!records.iter().any(|record| matches!(record["label"].as_str(),
            Some("test.nested" | "test.callback.existing" | "test.lexical.existing"))));
        let performance = crate::performance::snapshot();
        for name in ["test.callback", "test.lexical", "test.callback.panic", "test.lexical.panic"] {
            let own = performance["requests"].as_array().unwrap().iter().find(|row| row["kind"] == name).unwrap();
            assert_eq!(own["count"], if name.ends_with(".panic") { 0 } else { 1 });
        }
    }

    #[tokio::test]
    async fn busy_future_accrues_cpu_but_sleep_does_not() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build()).build();
        let metrics = instruments(&provider.meter("task-cpu-test"));
        measured("test.busy", async {
            let started = thread_cpu().unwrap();
            while thread_cpu().unwrap().saturating_sub(started) < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
        }, &metrics).await;
        measured("test.sleep", tokio::time::sleep(Duration::from_millis(100)), &metrics).await;
        provider.force_flush().unwrap();
        let exported = exporter.get_finished_metrics().unwrap();
        let cpu = exported.iter().flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "st.task.cpu_time")
            .and_then(|metric| match metric.data() {
                AggregatedMetrics::F64(MetricData::Sum(sum)) => Some(sum),
                _ => None,
            }).expect("CPU counter is exported");
        let value = |name: &'static str| cpu.data_points().filter(|point| point.attributes().any(|attribute| {
            attribute.key.as_str() == "st.task" && attribute.value == opentelemetry::Value::from(name)
        })).map(|point| point.value()).sum::<f64>();
        assert!(value("test.busy") >= 0.02);
        assert!(value("test.sleep") < 0.01);
        let polls = exported.iter().flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "st.task.polls")
            .and_then(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => Some(sum),
                _ => None,
            }).expect("poll counter is exported");
        assert!(polls.data_points().map(|point| point.value()).sum::<u64>() >= 3);
    }

    #[test]
    fn samples_batch_locally_and_nested_scopes_do_not_double_count() {
        let metrics = instruments(&opentelemetry::global::meter("task-cpu-nesting-test"));
        let mut outer = Accumulator::new("test.outer", &metrics);
        let mut inner = Accumulator::new("test.inner", &metrics);
        // Prevent scheduler delays from turning this local-buffer test into a timed flush.
        outer.flushed = Instant::now() + Duration::from_secs(60);
        inner.flushed = outer.flushed;
        {
            let _outer = Sample::start(&mut outer);
            let _inner = Sample::start(&mut inner);
            let started = thread_cpu().unwrap();
            while thread_cpu().unwrap().saturating_sub(started) < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
        }
        assert!(outer.cpu >= Duration::from_millis(20));
        assert_eq!(inner.cpu, Duration::ZERO);
        assert_eq!(outer.polls, 1);
        assert_eq!(inner.polls, 1);
        assert!(!MEASURING.with(Cell::get));
        outer.flush();
        assert_eq!(outer.cpu, Duration::ZERO);
        assert_eq!(outer.polls, 0);
    }

    #[test]
    fn unwinding_restores_thread_sampling_scope() {
        let metrics = instruments(&opentelemetry::global::meter("task-cpu-unwind-test"));
        let mut accumulator = Accumulator::new("test.unwind", &metrics);
        let _panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _sample = Sample::start(&mut accumulator);
            panic!("exercise sample unwinding");
        }));
        assert!(!MEASURING.with(Cell::get));
        assert_eq!(accumulator.polls, 1);
    }
}
