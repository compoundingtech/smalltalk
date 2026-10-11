//! One root per reconciler operation; bounded labels alongside the existing meter.
use opentelemetry::{
    KeyValue,
    metrics::{Counter, Histogram},
};
use std::sync::{
    LazyLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum WakeCause {
    Api,
    ReplicationReceive,
    Deadline,
    TimerRestart,
    TimerStepTimeout,
    TimerResumeVerification,
    TimerGateTimeout,
    TimerGateRecheck,
    TimerGate,
    TimerGatePoll,
    TimerLlmGate,
    FileWatch,
    Reconciler,
    RecorderReceipts,
    Startup,
    Continuation,
    Other,
}
impl WakeCause {
    pub(crate) const ALL: [Self; 17] = [
        Self::Api,
        Self::ReplicationReceive,
        Self::Deadline,
        Self::TimerRestart,
        Self::TimerStepTimeout,
        Self::TimerResumeVerification,
        Self::TimerGateTimeout,
        Self::TimerGateRecheck,
        Self::TimerGate,
        Self::TimerGatePoll,
        Self::TimerLlmGate,
        Self::FileWatch,
        Self::Reconciler,
        Self::RecorderReceipts,
        Self::Startup,
        Self::Continuation,
        Self::Other,
    ];
}
impl From<&str> for WakeCause {
    fn from(source: &str) -> Self {
        match source {
            "api" => Self::Api,
            "replication receive" => Self::ReplicationReceive,
            "deadline" => Self::Deadline,
            "timer restart" => Self::TimerRestart,
            "timer step-timeout" => Self::TimerStepTimeout,
            "timer resume-verification" => Self::TimerResumeVerification,
            "timer gate-timeout" => Self::TimerGateTimeout,
            "timer gate-recheck" => Self::TimerGateRecheck,
            "timer gate" => Self::TimerGate,
            "timer gate-poll" => Self::TimerGatePoll,
            "timer llm-gate" => Self::TimerLlmGate,
            "file watch" => Self::FileWatch,
            "reconciler" => Self::Reconciler,
            "recorder receipts" => Self::RecorderReceipts,
            "startup" => Self::Startup,
            "continuation" => Self::Continuation,
            _ => Self::Other,
        }
    }
}
impl WakeCause {
    pub(crate) const fn legacy_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::ReplicationReceive => "replication receive",
            Self::Deadline => "deadline",
            Self::TimerRestart => "timer restart",
            Self::TimerStepTimeout => "timer step-timeout",
            Self::TimerResumeVerification => "timer resume-verification",
            Self::TimerGateTimeout => "timer gate-timeout",
            Self::TimerGateRecheck => "timer gate-recheck",
            Self::TimerGate => "timer gate",
            Self::TimerGatePoll => "timer gate-poll",
            Self::TimerLlmGate => "timer llm-gate",
            Self::FileWatch => "file watch",
            Self::Reconciler => "reconciler",
            Self::RecorderReceipts => "recorder receipts",
            Self::Startup => "startup",
            Self::Continuation => "continuation",
            Self::Other => "other",
        }
    }
    pub(crate) const fn otel_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::ReplicationReceive => "replication_receive",
            Self::Deadline => "deadline",
            Self::TimerRestart => "timer_restart",
            Self::TimerStepTimeout => "timer_step_timeout",
            Self::TimerResumeVerification => "timer_resume_verification",
            Self::TimerGateTimeout => "timer_gate_timeout",
            Self::TimerGateRecheck => "timer_gate_recheck",
            Self::TimerGate => "timer_gate",
            Self::TimerGatePoll => "timer_gate_poll",
            Self::TimerLlmGate => "timer_llm_gate",
            Self::FileWatch => "file_watch",
            Self::Reconciler => "reconciler",
            Self::RecorderReceipts => "recorder_receipts",
            Self::Startup => "startup",
            Self::Continuation => "continuation",
            Self::Other => "other",
        }
    }
}
static PENDING: AtomicUsize = AtomicUsize::new(WakeCause::Startup as usize);
static ATTRIBUTES: LazyLock<[[KeyValue; 1]; WakeCause::ALL.len()]> =
    LazyLock::new(|| WakeCause::ALL.map(|cause| [KeyValue::new("cause", cause.otel_str())]));
static TASKS: LazyLock<[[KeyValue; 1]; 2]> = LazyLock::new(|| {
    [
        [KeyValue::new("task", "pass")],
        [KeyValue::new("task", "deadline")],
    ]
});
struct Instruments {
    wakes: Counter<u64>,
    duration: Histogram<f64>,
}
static METRICS: LazyLock<Instruments> = LazyLock::new(|| {
    let meter = opentelemetry::global::meter("st3.reconcile");
    Instruments {
        wakes: meter
            .u64_counter("st.reconcile.wakes")
            .with_unit("{wake}")
            .build(),
        duration: meter
            .f64_histogram("st.reconcile.pass.duration")
            .with_unit("s")
            .build(),
    }
});
pub(crate) fn init() {
    LazyLock::force(&ATTRIBUTES);
    LazyLock::force(&TASKS);
    LazyLock::force(&METRICS);
    smallclaims::fifo::init(&opentelemetry::global::meter("st3.fifo"));
}
/// Notify coalesces wakes: the next pass carries the last recorded cause, not a request parent.
pub(crate) fn record_wake(cause: WakeCause, detail: Option<&str>) {
    crate::performance::record_wake(cause.legacy_str(), detail);
    PENDING.store(cause as usize, Ordering::Relaxed);
    if crate::otel::metrics_enabled() {
        METRICS.wakes.add(1, &ATTRIBUTES[cause as usize]);
    }
}
pub(crate) fn take_wake() -> WakeCause {
    WakeCause::ALL[PENDING.swap(WakeCause::Other as usize, Ordering::Relaxed)]
}

/// Fault isolation lets the pass continue, but its root must still be retained as an error.
pub(crate) fn record_error() {
    if !crate::otel::export_enabled() {
        return;
    }
    let span = tracing::Span::current();
    if span.metadata().is_some_and(|metadata| {
        matches!(
            metadata.name(),
            "st.reconcile_pass" | "st.reconcile_deadline"
        )
    }) {
        span.set_status(opentelemetry::trace::Status::error("reconcile failed"));
    }
}

pub(crate) struct Operation {
    started: Option<Instant>,
    task: usize,
    pub(crate) span: tracing::Span,
}
impl Operation {
    pub(crate) fn new(deadline: bool, cause: WakeCause) -> Self {
        let span = if !crate::otel::export_enabled() {
            tracing::Span::none()
        } else {
            let span = if deadline {
                tracing::info_span!(parent: None, "st.reconcile_deadline",
                    "span.label" = "deadline", "st.reconcile.wake_cause" = cause.otel_str())
            } else {
                tracing::info_span!(parent: None, "st.reconcile_pass",
                    "span.label" = "pass", "st.reconcile.wake_cause" = cause.otel_str(),
                    "st.reconcile.items" = tracing::field::Empty)
            };
            // parent: None detaches tracing ancestry; empty context also detaches ambient OTel.
            span.set_parent(opentelemetry::Context::new());
            span
        };
        Self {
            started: crate::otel::metrics_enabled().then(Instant::now),
            task: usize::from(deadline),
            span,
        }
    }
    pub(crate) fn error(&self) {
        self.span
            .set_status(opentelemetry::trace::Status::error("reconcile failed"));
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.error();
        }
        if let Some(started) = self.started {
            METRICS
                .duration
                .record(started.elapsed().as_secs_f64(), &TASKS[self.task]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn otel_existing_wake_sources_are_closed() {
        let sources = [
            "api",
            "replication receive",
            "deadline",
            "timer restart",
            "timer step-timeout",
            "timer resume-verification",
            "timer gate-timeout",
            "timer gate-recheck",
            "timer gate",
            "timer gate-poll",
            "timer llm-gate",
            "file watch",
            "reconciler",
            "recorder receipts",
        ];
        for source in sources {
            let cause = WakeCause::from(source);
            assert_ne!(cause, WakeCause::Other, "{source}");
            assert_eq!(cause.legacy_str(), source);
        }
        assert_eq!(WakeCause::from("unknown source"), WakeCause::Other);
        let otel_values = [
            "api", "replication_receive", "deadline", "timer_restart", "timer_step_timeout",
            "timer_resume_verification", "timer_gate_timeout", "timer_gate_recheck",
            "timer_gate", "timer_gate_poll", "timer_llm_gate", "file_watch", "reconciler",
            "recorder_receipts", "startup", "continuation", "other",
        ];
        for (index, cause) in WakeCause::ALL.into_iter().enumerate() {
            assert_eq!(cause as usize, index);
            assert_eq!(WakeCause::from(cause.legacy_str()), cause);
            assert_eq!(cause.otel_str(), otel_values[index]);
            assert_eq!(ATTRIBUTES[index][0], KeyValue::new("cause", otel_values[index]));
        }
    }
}
