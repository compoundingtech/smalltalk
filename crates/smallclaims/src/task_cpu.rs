//! Thread CPU consumed by daemon tasks, excluding time parked between polls.
use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

struct Instruments {
    cpu: Counter<f64>,
    polls: Counter<u64>,
}

static METRICS: LazyLock<Instruments> =
    LazyLock::new(|| instruments(&opentelemetry::global::meter("st3")));

fn instruments(meter: &opentelemetry::metrics::Meter) -> Instruments {
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

struct Sample<'a, 'm> {
    started: Option<Duration>,
    nested: bool,
    accumulator: &'a mut Accumulator<'m>,
}

impl<'a, 'm> Sample<'a, 'm> {
    fn start(accumulator: &'a mut Accumulator<'m>) -> Self {
        let nested = MEASURING.with(|active| active.replace(true));
        Self { started: (!nested).then(thread_cpu).flatten(), nested, accumulator }
    }
}

impl Drop for Sample<'_, '_> {
    fn drop(&mut self) {
        let ended = (!self.nested).then(thread_cpu).flatten();
        MEASURING.with(|active| active.set(self.nested));
        self.accumulator.polls += 1;
        if let Some(elapsed) = self.started.zip(ended).and_then(|(start, end)| end.checked_sub(start)) {
            self.accumulator.cpu += elapsed;
        }
        if self.accumulator.flushed.elapsed() >= Duration::from_secs(1) {
            self.accumulator.flush();
        }
    }
}

/// Measure each poll on its current thread; never retain a clock sample across an await.
/// The name must describe an operation, never an instance, seat, peer, or request.
/// Samples accumulate with the future and reach OTel at most once a second while
/// polling, plus a final flush on completion or cancellation. Nested wrappers on
/// the same thread count polls but leave their CPU to the outermost wrapper.
pub async fn account<F: Future>(name: &'static str, future: F) -> F::Output {
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
    let mut accumulator = Accumulator::new(name, &METRICS);
    let _sample = Sample::start(&mut accumulator);
    crate::profile::task(name, work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};

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
